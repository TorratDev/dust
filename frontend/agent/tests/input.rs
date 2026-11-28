//! Agent input reaches the emulated hardware.

mod common;

use common::test_rom;
use dust_agent::{InputStep, Keys, Session, SessionOptions, LID_CLOSED};
use dust_core::cpu::{arm7, arm9, bus::DebugCpuAccess};

const KEYINPUT: u32 = 0x0400_0130;
const EXTKEYIN: u32 = 0x0400_0136;

fn session() -> Session {
    Session::new(test_rom(), &SessionOptions::default()).unwrap()
}

fn keyinput(session: &mut Session) -> u16 {
    arm9::bus::read_16::<DebugCpuAccess, _>(session.emu(), KEYINPUT)
}

fn extkeyin(session: &mut Session) -> u16 {
    arm7::bus::read_16::<DebugCpuAccess, _>(session.emu(), EXTKEYIN)
}

#[test]
fn hold_a_one_frame_shows_in_keyinput() {
    let mut session = session();
    session.step(1).unwrap();
    assert_eq!(
        keyinput(&mut session) & 1,
        1,
        "A released before holding it"
    );

    session.hold(Keys::A);
    session.step(1).unwrap();
    // KEYINPUT bits are 0 while pressed.
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3FE);

    session.release(Keys::A);
    session.step(1).unwrap();
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3FF);
}

#[test]
fn press_releases_afterwards() {
    let mut session = session();
    session.press(Keys::START, 2).unwrap();
    assert_eq!(session.held(), Keys::empty());
    session.step(1).unwrap();
    assert_eq!(keyinput(&mut session) & 1 << 3, 1 << 3);
}

#[test]
fn keypad_mask_x_and_lid() {
    let mut session = session();
    // py-desmume: X = 1 << 10, lid = 1 << 14.
    session.set_keypad(1 << 10 | 1 << 14);
    assert_eq!(session.held(), Keys::X | LID_CLOSED);
    session.step(1).unwrap();
    let ext = extkeyin(&mut session);
    assert_eq!(ext & 1, 0, "X pressed");
    assert_eq!(ext & 1 << 7, 1 << 7, "lid closed");

    session.release(Keys::X);
    assert_eq!(session.held(), LID_CLOSED);
}

#[test]
fn touch_sets_pen_down_and_validates() {
    let mut session = session();
    assert!(session.touch(256, 0, 1).is_err());
    session.touch(128, 96, 0).unwrap();
    session.step(1).unwrap();
    assert_eq!(extkeyin(&mut session) & 1 << 6, 0, "pen down");
    session.touch_end();
    session.step(1).unwrap();
    assert_eq!(extkeyin(&mut session) & 1 << 6, 1 << 6, "pen up");
}

#[test]
fn sequence_restores_held_input() {
    let mut session = session();
    session.hold(Keys::B);
    let status = session
        .sequence(&[
            InputStep {
                keys: Keys::A,
                touch: None,
                frames: 2,
            },
            InputStep {
                keys: Keys::empty(),
                touch: Some([10, 10]),
                frames: 3,
            },
        ])
        .unwrap();
    assert_eq!(status.frame, 5);
    assert_eq!(session.held(), Keys::B);
}
