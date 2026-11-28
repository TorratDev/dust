//! Breakpoints and write watchpoints against a real `dust-headless` (see `boot.rs` for what it
//! needs). The generated test ROM's ARM9 spins on `b .` at its entry point.

mod common;

use dust_client::{Cpu, HookKind, MemRange};
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

fn arm9_entry() -> u32 {
    let rom = std::fs::read(common::test_rom()).unwrap();
    u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap())
}

const CODE_ADDR: u32 = 0x0200_1000;
const STORE_ADDR: u32 = 0x0230_0000;
const STORE_VALUE: u32 = 0xCAFE_BABE;

/// Makes the ARM9 run a routine that stores `STORE_VALUE` at `STORE_ADDR` once, then spins.
async fn inject_store(session: &dust_client::Session) {
    let code: [u32; 6] = [
        0xE59F_0008, // ldr r0, [pc, #8]
        0xE59F_1008, // ldr r1, [pc, #8]
        0xE580_1000, // str r1, [r0]
        0xEAFF_FFFE, // b .
        STORE_ADDR,
        STORE_VALUE,
    ];
    let bytes: Vec<u8> = code.iter().flat_map(|word| word.to_le_bytes()).collect();
    session.write(Cpu::Arm9, CODE_ADDR, &bytes).await.unwrap();
    let cpsr = session.regs(Cpu::Arm9).await.unwrap().cpsr;
    session.set_reg(Cpu::Arm9, 16, cpsr | 0xC0).await.unwrap();
    session.set_reg(Cpu::Arm9, 15, CODE_ADDR).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn breakpoint_at_entry_fires_once_after_reset() {
    let entry = arm9_entry();
    let session = common::launch().await;
    let hits = Arc::new(Mutex::new(Vec::new()));
    let hits_ = Arc::clone(&hits);
    let id = session
        .on_exec(entry, move |emu, hit| {
            let hits = Arc::clone(&hits_);
            async move {
                // One-shot: the entry is a spin loop that would hit on every iteration. The
                // handler can use the stopped session.
                emu.break_remove(hit.id).await?;
                let insn = emu.read_u32(Cpu::Arm9, hit.addr).await?;
                hits.lock().unwrap().push((hit, insn));
                Ok(())
            }
        })
        .await
        .unwrap();
    assert_eq!(session.break_list().await.unwrap().len(), 1);
    session.reset().await.unwrap();

    let response = session.step_response(2).await.unwrap();
    assert_eq!(response.status.unwrap().frame, 2);
    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let (hit, insn) = &hits[0];
    assert_eq!(
        (hit.id, hit.kind(), hit.cpu(), hit.addr, hit.frame),
        (id, HookKind::Breakpoint, Cpu::Arm9, entry, 0)
    );
    assert_eq!(hit.regs.as_ref().unwrap().gprs[15], entry + 8);
    assert_eq!(*insn, 0xEAFF_FFFE);
    assert_eq!(response.hits, [hit.clone()]);
    assert!(session.break_list().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn write_watchpoint_fires_on_store() {
    let session = common::launch().await;
    let hits = Arc::new(Mutex::new(Vec::new()));
    let hits_ = Arc::clone(&hits);
    let id = session
        .on_write_with(
            Cpu::Arm9,
            STORE_ADDR,
            4,
            &[MemRange {
                addr: STORE_ADDR,
                len: 4,
            }],
            move |_emu, hit| {
                hits_.lock().unwrap().push(hit);
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
    inject_store(&session).await;
    session.cycle(2).await.unwrap();

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(
        (hit.id, hit.kind(), hit.addr, hit.size, hit.value),
        (id, HookKind::WriteWatchpoint, STORE_ADDR, 4, STORE_VALUE)
    );
    assert_eq!(hit.reads, [STORE_VALUE.to_le_bytes().to_vec()]);
    assert_eq!(hit.regs.as_ref().unwrap().gprs[1], STORE_VALUE);
}

#[tokio::test(flavor = "multi_thread")]
async fn hits_without_subscriber_dont_stop() {
    let session = common::launch().await;
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, &[])
        .await
        .unwrap();
    inject_store(&session).await;
    let response = session.step_response(1).await.unwrap();
    assert_eq!(response.hits.len(), 1);
    assert_eq!(response.hits[0].value, STORE_VALUE);
    // Nothing is stopped.
    assert!(session.continue_().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn per_hit_round_trip() {
    const HITS: u32 = 2000;
    let entry = arm9_entry();
    let session = common::launch().await;
    let count = Arc::new(AtomicU32::new(0));
    let count_ = Arc::clone(&count);
    session
        .on_exec(entry, move |emu, hit| {
            let count = Arc::clone(&count_);
            async move {
                if count.fetch_add(1, Ordering::Relaxed) + 1 == HITS {
                    emu.break_remove(hit.id).await?;
                }
                Ok(())
            }
        })
        .await
        .unwrap();
    // Warm up the connection, then time a frame with no hooks for the baseline.
    let no_hooks = frame_time(&session).await;
    let start = Instant::now();
    session.cycle(1).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(count.load(Ordering::Relaxed), HITS);
    let per_hit = (elapsed.saturating_sub(no_hooks)).as_secs_f64() * 1e6 / HITS as f64;
    eprintln!(
        "gRPC breakpoint round trip (event -> handler -> Continue): {per_hit:.1} us per hit \
         ({HITS} hits + 1 frame in {elapsed:?}, frame alone {no_hooks:?})"
    );
}

/// Time of one frame on a fresh session with no hooks (after a warm-up frame).
async fn frame_time(session: &dust_client::Session) -> std::time::Duration {
    let other = session
        .client()
        .create_session(common::test_rom(), &Default::default())
        .await
        .unwrap();
    other.cycle(1).await.unwrap();
    let start = Instant::now();
    other.cycle(1).await.unwrap();
    let elapsed = start.elapsed();
    other.destroy().await.unwrap();
    elapsed
}
