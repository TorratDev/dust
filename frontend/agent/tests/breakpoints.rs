//! Breakpoints and write watchpoints against the generated test ROM: its ARM9 code at 0x02000000
//! stores 0x5A as a word at 0x02001000 (`str` at 0x02000008), then spins at 0x0200000C.

mod common;

use common::test_rom;
use dust_agent::{Cpu, Error, HookHit, HookKind, MemRange, Session, SessionOptions};
use std::{cell::RefCell, ops::ControlFlow, rc::Rc};

const ARM9_ENTRY: u32 = 0x0200_0000;
const ARM9_STORE: u32 = 0x0200_0008;
const ARM9_SPIN: u32 = 0x0200_000C;
const STORE_ADDR: u32 = 0x0200_1000;
const STORE_VALUE: u32 = 0x5A;
const ARM7_ENTRY: u32 = 0x0238_0000;

fn session() -> Session {
    if std::env::var_os("DUST_TEST_ROM").is_some() {
        eprintln!("note: these tests expect the generated test ROM");
    }
    Session::new(test_rom(), &SessionOptions::default()).unwrap()
}

/// Records every hit.
fn record(session: &mut Session) -> Rc<RefCell<Vec<HookHit>>> {
    let hits = Rc::new(RefCell::new(Vec::new()));
    let hits_ = Rc::clone(&hits);
    session.set_hit_handler(Some(Box::new(move |_session, hit| {
        hits_.borrow_mut().push(hit.clone());
        ControlFlow::Continue(())
    })));
    hits
}

#[test]
fn entry_breakpoint_fires_once_after_reset() {
    let mut session = session();
    let id = session
        .break_add(
            Cpu::Arm9,
            ARM9_ENTRY,
            vec![MemRange {
                addr: ARM9_ENTRY,
                len: 4,
            }],
        )
        .unwrap();
    session.reset().unwrap();
    let hits = record(&mut session);
    session.step(3).unwrap();

    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(hit.id, id);
    assert_eq!(hit.kind, HookKind::Breakpoint);
    assert_eq!(hit.cpu, Cpu::Arm9);
    assert_eq!(hit.addr, ARM9_ENTRY);
    assert_eq!(hit.frame, 0);
    assert_eq!(hit.regs.gprs[15], ARM9_ENTRY);
    // ldr r0, [pc, #8]
    assert_eq!(hit.reads, vec![0xE59F_0008u32.to_le_bytes().to_vec()]);
}

#[test]
fn write_watchpoint_fires_on_store() {
    let mut session = session();
    let id = session
        .watch_write_add(
            Cpu::Arm9,
            STORE_ADDR,
            4,
            vec![MemRange {
                addr: STORE_ADDR,
                len: 4,
            }],
        )
        .unwrap();
    let hits = record(&mut session);
    session.step(2).unwrap();

    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(hit.id, id);
    assert_eq!(hit.kind, HookKind::WriteWatchpoint);
    assert_eq!(hit.addr, STORE_ADDR);
    assert_eq!(hit.value, STORE_VALUE);
    // Stopped right after the store.
    assert_eq!(hit.regs.gprs[15], ARM9_STORE + 4);
    assert_eq!(hit.regs.gprs[0], STORE_ADDR);
    assert_eq!(hit.reads, vec![STORE_VALUE.to_le_bytes().to_vec()]);
}

#[test]
fn byte_watchpoint_inside_word_store() {
    let mut session = session();
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR + 2, 1, Vec::new())
        .unwrap();
    // Not watched: just past the stored word.
    session
        .watch_write_add(Cpu::Arm9, STORE_ADDR + 4, 4, Vec::new())
        .unwrap();
    let hits = record(&mut session);
    session.step(1).unwrap();
    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!((hits[0].addr, hits[0].value), (STORE_ADDR, STORE_VALUE));
}

#[test]
fn spin_loop_breakpoint_hits_every_iteration_and_remove_stops_it() {
    let mut session = session();
    let id = session.break_add(Cpu::Arm9, ARM9_SPIN, Vec::new()).unwrap();
    let count = Rc::new(RefCell::new(0));
    let count_ = Rc::clone(&count);
    session.set_hit_handler(Some(Box::new(move |session, hit| {
        *count_.borrow_mut() += 1;
        if *count_.borrow() == 100 {
            session.break_remove(hit.id).unwrap();
        }
        ControlFlow::Continue(())
    })));
    session.step(2).unwrap();
    assert_eq!(*count.borrow(), 100);
    assert!(session.hooks().is_empty());
    assert!(matches!(
        session.break_remove(id),
        Err(Error::NoSuchHook(_))
    ));
}

#[test]
fn arm7_breakpoint_and_list() {
    let mut session = session();
    let a = session
        .break_add(Cpu::Arm7, ARM7_ENTRY, Vec::new())
        .unwrap();
    let b = session
        .watch_write_add(Cpu::Arm9, STORE_ADDR, 4, Vec::new())
        .unwrap();
    assert_eq!(
        session
            .hooks()
            .iter()
            .map(|hook| (hook.id, hook.kind))
            .collect::<Vec<_>>(),
        [(a, HookKind::Breakpoint), (b, HookKind::WriteWatchpoint)]
    );
    session.watch_write_remove(b).unwrap();
    // The ARM7 entry is its spin loop: stop after the first hit.
    let hits = Rc::new(RefCell::new(Vec::new()));
    let hits_ = Rc::clone(&hits);
    session.set_hit_handler(Some(Box::new(move |session, hit| {
        hits_.borrow_mut().push(hit.clone());
        session.break_remove(hit.id).unwrap();
        ControlFlow::Continue(())
    })));
    session.step(1).unwrap();
    let hits = hits.borrow();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!((hits[0].cpu, hits[0].addr), (Cpu::Arm7, ARM7_ENTRY));
    assert_eq!(hits[0].regs.gprs[15], ARM7_ENTRY);
}

#[test]
fn handler_cannot_step_and_can_interrupt() {
    let mut session = session();
    session
        .break_add(Cpu::Arm9, ARM9_ENTRY, Vec::new())
        .unwrap();
    session.set_hit_handler(Some(Box::new(|session, _hit| {
        assert!(matches!(session.step(1), Err(Error::StoppedAtHook)));
        assert!(matches!(session.reset(), Err(Error::StoppedAtHook)));
        session.request_continue().unwrap();
        assert!(session.take_continue());
        ControlFlow::Break(())
    })));
    assert!(matches!(session.step(1), Err(Error::Interrupted)));
    assert!(matches!(session.request_continue(), Err(Error::NotStopped)));
    // The interrupted frame finishes on the next step.
    session.set_hit_handler(None);
    assert_eq!(session.step(1).unwrap().frame, 1);
}
