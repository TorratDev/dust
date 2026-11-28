//! Breakpoints and write watchpoints. Uses the ROM from `tools/fetch_test_rom.sh` (generated if
//! missing) or `$DUST_TEST_ROM`; the generated ROM's ARM9 spins on `b .` at its entry point.

use dust_agent::{Cpu, HookHit, HookKind, MemRange, Session, SessionOptions};
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    time::Instant,
};

fn test_rom() -> PathBuf {
    if let Some(rom) = std::env::var_os("DUST_TEST_ROM") {
        return rom.into();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rom = root.join("tests/roms/test.nds");
    if !rom.exists() {
        let status = Command::new("bash")
            .arg(root.join("tools/fetch_test_rom.sh"))
            .status()
            .expect("couldn't run tools/fetch_test_rom.sh");
        assert!(status.success(), "tools/fetch_test_rom.sh failed");
    }
    rom
}

fn session() -> Session {
    Session::new(
        test_rom(),
        &SessionOptions {
            rtc_unix_time: Some(0),
            ..Default::default()
        },
    )
    .unwrap()
}

fn arm9_entry() -> u32 {
    let rom = fs::read(test_rom()).unwrap();
    u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap())
}

/// Collects hits; with `one_shot`, removes each breakpoint on its first hit.
fn recording_handler(session: &mut Session, one_shot: bool) -> Rc<RefCell<Vec<HookHit>>> {
    let hits = Rc::new(RefCell::new(Vec::new()));
    let hits_ = Rc::clone(&hits);
    session.set_hit_handler(Some(Box::new(move |session, hit| {
        assert!(session.in_hit_handler());
        hits_.borrow_mut().push(hit.clone());
        if one_shot && hit.kind == HookKind::Breakpoint {
            session.break_remove(hit.id).unwrap();
        }
    })));
    hits
}

const CODE_ADDR: u32 = 0x0200_1000;
const STORE_ADDR: u32 = 0x0230_0000;
const STORE_VALUE: u32 = 0xCAFE_BABE;

/// Puts an ARM routine at `CODE_ADDR` that stores `STORE_VALUE` at `STORE_ADDR` once, then spins,
/// and makes the ARM9 run it with interrupts off.
fn inject_store(session: &mut Session) {
    let code: [u32; 6] = [
        0xE59F_0008, // ldr r0, [pc, #8]
        0xE59F_1008, // ldr r1, [pc, #8]
        0xE580_1000, // str r1, [r0]
        0xEAFF_FFFE, // b .
        STORE_ADDR,
        STORE_VALUE,
    ];
    let bytes: Vec<u8> = code.iter().flat_map(|word| word.to_le_bytes()).collect();
    session.write(Cpu::Arm9, CODE_ADDR, &bytes).unwrap();
    let cpsr = session.regs(Cpu::Arm9).cpsr;
    session.set_reg(Cpu::Arm9, 16, cpsr | 0xC0).unwrap();
    session.set_reg(Cpu::Arm9, 15, CODE_ADDR).unwrap();
}

#[test]
fn breakpoint_at_entry_fires_once_after_reset() {
    let entry = arm9_entry();
    let mut session = session();
    let hits = recording_handler(&mut session, true);
    let id = session
        .break_add(
            Cpu::Arm9,
            entry,
            vec![MemRange {
                addr: entry,
                len: 4,
            }],
        )
        .unwrap();
    assert_eq!(session.hooks().len(), 1);
    session.reset().unwrap();

    let result = session.step(2).unwrap();
    assert_eq!(result.status.frame, 2);
    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(result.hits, *hits);
    let hit = &hits[0];
    assert_eq!(
        (hit.id, hit.kind, hit.cpu, hit.addr, hit.frame),
        (id, HookKind::Breakpoint, Cpu::Arm9, entry, 0)
    );
    // r15 reads 8 ahead in ARM state, and the instruction hasn't run yet.
    assert_eq!(hit.regs.gprs[15], entry + 8);
    // The registered read: the entry's `b .`.
    assert_eq!(hit.reads, [0xEAFF_FFFEu32.to_le_bytes().to_vec()]);
    assert!(session.hooks().is_empty());
}

#[test]
fn breakpoint_keeps_firing_in_a_loop_until_removed() {
    let entry = arm9_entry();
    let mut session = session();
    let id = session.break_add(Cpu::Arm9, entry, Vec::new()).unwrap();
    let count = Rc::new(RefCell::new(0u32));
    let count_ = Rc::clone(&count);
    session.set_hit_handler(Some(Box::new(move |session, hit| {
        *count_.borrow_mut() += 1;
        if *count_.borrow() == 1000 {
            session.break_remove(hit.id).unwrap();
        }
    })));
    let start = Instant::now();
    let result = session.step(1).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(*count.borrow(), 1000);
    assert_eq!(result.hits.len(), 1000);
    assert!(result
        .hits
        .iter()
        .all(|hit| hit.id == id && hit.addr == entry));
    eprintln!(
        "in-process breakpoint hit: {:.2} us per hit (1000 hits + 1 frame in {elapsed:?})",
        elapsed.as_secs_f64() * 1e6 / 1000.0
    );
}

#[test]
fn write_watchpoint_fires_on_store() {
    let mut session = session();
    let hits = recording_handler(&mut session, false);
    // Watch 2 bytes in the middle of the 4-byte store.
    let id = session
        .watch_write_add(
            Cpu::Arm9,
            STORE_ADDR + 1,
            2,
            vec![MemRange {
                addr: STORE_ADDR,
                len: 4,
            }],
        )
        .unwrap();
    // A watchpoint elsewhere must stay quiet.
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR + 0x100, 4, Vec::new())
        .unwrap();
    inject_store(&mut session);
    session.step(3).unwrap();

    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(
        (hit.id, hit.kind, hit.addr, hit.size, hit.value, hit.frame),
        (id, HookKind::WriteWatchpoint, STORE_ADDR, 4, STORE_VALUE, 0)
    );
    // Stopped right after the `str`: r15 is 8 past the `b .` that follows it.
    assert_eq!(hit.regs.gprs[15], CODE_ADDR + 12 + 8);
    assert_eq!(hit.regs.gprs[1], STORE_VALUE);
    assert_eq!(hit.reads, [STORE_VALUE.to_le_bytes().to_vec()]);
}

#[test]
fn removed_watchpoint_doesnt_fire_and_others_survive() {
    let mut session = session();
    let hits = recording_handler(&mut session, false);
    let a = session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, Vec::new())
        .unwrap();
    let b = session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 1, Vec::new())
        .unwrap();
    session.watch_write_remove(a).unwrap();
    assert!(session.watch_write_remove(a).is_err());
    assert!(session.break_remove(b).is_err(), "kinds have separate ids");
    inject_store(&mut session);
    session.step(1).unwrap();
    let ids: Vec<_> = hits.borrow().iter().map(|hit| hit.id).collect();
    assert_eq!(ids, [b]);
}

#[test]
fn hooks_survive_state_loads_and_handler_cant_step() {
    let mut session = session();
    let state = session.save_state_bytes(false).unwrap();
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, Vec::new())
        .unwrap();
    session.load_state_bytes(&state.state, None).unwrap();
    let stepped = Rc::new(RefCell::new(None));
    let stepped_ = Rc::clone(&stepped);
    session.set_hit_handler(Some(Box::new(move |session, _hit| {
        *stepped_.borrow_mut() = Some(session.step(1).is_err());
    })));
    inject_store(&mut session);
    let result = session.step(1).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(*stepped.borrow(), Some(true));
}

#[test]
fn hits_without_handler_are_returned() {
    let mut session = session();
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, Vec::new())
        .unwrap();
    inject_store(&mut session);
    let result = session.step(1).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].value, STORE_VALUE);
    assert_eq!(
        session.read(Cpu::Arm9, STORE_ADDR, 4).unwrap(),
        STORE_VALUE.to_le_bytes()
    );
}

#[test]
fn invalid_hooks_are_rejected() {
    let mut session = session();
    assert!(session
        .watch_write_add(Cpu::Arm9, 0, 0, Vec::new())
        .is_err());
    assert!(session
        .watch_write_add(Cpu::Arm9, 0xFFFF_FFFF, 2, Vec::new())
        .is_err());
    assert!(session.break_remove(42).is_err());
}
