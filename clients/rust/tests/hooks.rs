//! Breakpoints and write watchpoints through a real `dust-headless` (see `common/mod.rs`), on the
//! generated test ROM: its ARM9 code at 0x02000000 stores 0x5A as a word at 0x02001000, then
//! spins with `b .` at 0x0200000C.
//!
//! `hit_round_trip_time` prints per-hit round-trip times; run with `--nocapture` to see them.

mod common;

use common::launch;
use dust_client::{pb, Cpu, Hit, HookKind, MemRange};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

const ARM9_ENTRY: u32 = 0x0200_0000;
const ARM9_STORE: u32 = 0x0200_0008;
const ARM9_SPIN: u32 = 0x0200_000C;
const STORE_ADDR: u32 = 0x0200_1000;
const STORE_VALUE: u32 = 0x5A;

type Hits = Arc<Mutex<Vec<Hit>>>;

/// A hit handler that records every hit.
fn recorder() -> (
    Hits,
    impl FnMut(dust_client::Session, Hit) -> std::future::Ready<()> + Send + 'static,
) {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let hits_ = Arc::clone(&hits);
    (hits, move |_emu, hit| {
        hits_.lock().unwrap().push(hit);
        std::future::ready(())
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn entry_breakpoint_fires_once_after_reset() {
    let session = launch().await;
    let (hits, handler) = recorder();
    let reads = [MemRange {
        addr: ARM9_ENTRY,
        len: 4,
    }];
    let hook = session
        .on_exec(Cpu::Arm9, ARM9_ENTRY, &reads, handler)
        .await
        .unwrap();
    assert_eq!(hook.kind, HookKind::Breakpoint);
    session.reset().await.unwrap();
    assert_eq!(session.cycle(3).await.unwrap().frame, 3);

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!((hit.id, hit.kind, hit.cpu), (hook.id, hook.kind, Cpu::Arm9));
    assert_eq!(hit.addr, ARM9_ENTRY);
    assert_eq!(hit.frame, 0);
    assert_eq!(hit.regs.gprs[15], ARM9_ENTRY);
    assert_eq!(hit.reads, vec![0xE59F_0008u32.to_le_bytes().to_vec()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn write_watchpoint_fires_on_store() {
    let session = launch().await;
    let (hits, handler) = recorder();
    let reads = [MemRange {
        addr: STORE_ADDR,
        len: 4,
    }];
    let hook = session
        .on_write(Cpu::Arm9, STORE_ADDR, 4, &reads, handler)
        .await
        .unwrap();
    session.cycle(2).await.unwrap();

    let hits = hits.lock().unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!((hit.id, hit.kind), (hook.id, HookKind::WriteWatchpoint));
    assert_eq!((hit.addr, hit.value), (STORE_ADDR, STORE_VALUE));
    assert_eq!(hit.regs.gprs[15], ARM9_STORE + 4);
    assert_eq!(hit.reads, vec![STORE_VALUE.to_le_bytes().to_vec()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_remove_and_errors() {
    let session = launch().await;
    let bp = session
        .break_add(Cpu::Arm7, 0x0238_0000, &[])
        .await
        .unwrap();
    let wp = session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, &[])
        .await
        .unwrap();
    let hooks = session.hooks().await.unwrap();
    assert_eq!(
        hooks
            .iter()
            .map(|hook| (hook.id, hook.kind(), hook.cpu()))
            .collect::<Vec<_>>(),
        [
            (bp.id, HookKind::Breakpoint, Cpu::Arm7),
            (wp.id, HookKind::WriteWatchpoint, Cpu::Arm9)
        ]
    );
    session.remove_hook(bp).await.unwrap();
    session.remove_hook(wp).await.unwrap();
    assert!(session.hooks().await.unwrap().is_empty());

    let code = |result: dust_client::Result<()>| match result {
        Err(dust_client::Error::Rpc(status)) => status.code(),
        other => panic!("unexpected result: {other:?}"),
    };
    assert_eq!(code(session.remove_hook(bp).await), tonic::Code::NotFound);
    assert_eq!(
        code(session.continue_hit().await),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(
        code(
            session
                .watch_write_add(Cpu::Arm9, STORE_ADDR, 0, &[])
                .await
                .map(|_| ())
        ),
        tonic::Code::InvalidArgument
    );
}

/// Destroying a session stopped at a hit aborts the pending step instead of hanging.
#[tokio::test(flavor = "multi_thread")]
async fn destroy_while_stopped() {
    let session = launch().await;
    let keep_alive = session.clone();
    let mut events = session.events().await.unwrap();
    session.break_add(Cpu::Arm9, ARM9_ENTRY, &[]).await.unwrap();
    let stepping = tokio::spawn({
        let session = session.clone();
        async move { session.cycle(1).await }
    });
    let event = events.message().await.unwrap().unwrap();
    assert!(matches!(event.kind, Some(pb::event::Kind::HookHit(_))));
    session.destroy().await.unwrap();
    assert!(stepping.await.unwrap().is_err());
    drop(keep_alive);
}

fn summarize(label: &str, times_us: &mut [f64]) {
    times_us.sort_by(f64::total_cmp);
    let mean = times_us.iter().sum::<f64>() / times_us.len() as f64;
    let pct = |p: f64| times_us[((times_us.len() - 1) as f64 * p) as usize];
    eprintln!(
        "{label}: {} hits, mean {mean:.1} us, p50 {:.1} us, p99 {:.1} us, max {:.1} us",
        times_us.len(),
        pct(0.5),
        pct(0.99),
        pct(1.0),
    );
}

/// Per-hit round trip: the emulator stops, the event reaches the client, the client continues.
/// The spin loop breakpoint fires on every iteration.
#[tokio::test(flavor = "multi_thread")]
async fn hit_round_trip_time() {
    const HITS: usize = 2000;
    let session = launch().await;

    // Raw API: time between consecutive hit events, each answered with Continue right away.
    let mut events = session.events().await.unwrap();
    let hook = session.break_add(Cpu::Arm9, ARM9_SPIN, &[]).await.unwrap();
    let stepping = tokio::spawn({
        let session = session.clone();
        async move { session.cycle(1).await }
    });
    let mut times = Vec::with_capacity(HITS);
    let mut last = None;
    for _ in 0..HITS {
        let event = events.message().await.unwrap().unwrap();
        let now = Instant::now();
        if let Some(last) = last {
            times.push(now.duration_since(last).as_secs_f64() * 1e6);
        }
        last = Some(now);
        let Some(pb::event::Kind::HookHit(hit)) = event.kind else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(hit.addr, ARM9_SPIN);
        session.continue_hit().await.unwrap();
    }
    // The last Continue lets the emulator hit again; remove the hook from the stopped state.
    events.message().await.unwrap().unwrap();
    session.remove_hook(hook).await.unwrap();
    session.continue_hit().await.unwrap();
    stepping.await.unwrap().unwrap();
    summarize("events + continue", &mut times);
    drop(events);

    // `on_exec` with a 4-byte read registered on the hook.
    let times = Arc::new(Mutex::new(Vec::with_capacity(HITS)));
    let last = Arc::new(Mutex::new(None::<Instant>));
    let reads = [MemRange {
        addr: STORE_ADDR,
        len: 4,
    }];
    session
        .on_exec(Cpu::Arm9, ARM9_SPIN, &reads, {
            let (times, last) = (Arc::clone(&times), Arc::clone(&last));
            move |emu, hit| {
                let now = Instant::now();
                let mut times_ = times.lock().unwrap();
                if let Some(last) = last.lock().unwrap().replace(now) {
                    times_.push(now.duration_since(last).as_secs_f64() * 1e6);
                }
                let done = times_.len() + 1 >= HITS;
                drop(times_);
                assert_eq!(hit.reads[0], STORE_VALUE.to_le_bytes());
                async move {
                    if done {
                        emu.remove_hook(dust_client::HookId {
                            id: hit.id,
                            kind: hit.kind,
                        })
                        .await
                        .unwrap();
                    }
                }
            }
        })
        .await
        .unwrap();
    session.cycle(1).await.unwrap();
    let mut times = std::mem::take(&mut *times.lock().unwrap());
    assert_eq!(times.len() + 1, HITS);
    summarize("on_exec", &mut times);

    // For scale: a frame with no hooks.
    let start = Instant::now();
    session.cycle(60).await.unwrap();
    eprintln!(
        "no hooks: {:.0} us per frame",
        start.elapsed().as_secs_f64() * 1e6 / 60.0
    );
}
