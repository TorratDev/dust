//! Agent input reaches the emulated hardware. Uses the ROM from `tools/fetch_test_rom.sh`
//! (generated if missing) or `$DUST_TEST_ROM`.

use dust_agent::{Control, InputStep, Keys, Session, SessionOptions, LID_CLOSED};
use dust_core::cpu::{arm7, arm9, bus::DebugCpuAccess};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

const KEYINPUT: u32 = 0x0400_0130;
const EXTKEYIN: u32 = 0x0400_0136;

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
    assert_eq!(status.status.frame, 5);
    assert_eq!(session.held(), Keys::B);
}

#[test]
fn control_mode_merges_human_and_agent_input() {
    let mut session = session();
    session.hold(Keys::A);
    session.set_human_input(Keys::B, None);

    // Agent control (the default) ignores the human.
    session.step(1).unwrap();
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3FE);

    session.set_control(Control::Human);
    session.step(1).unwrap();
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3FD);

    session.set_control(Control::Shared);
    session.step(1).unwrap();
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3FC);
    assert_eq!(session.status().control, Control::Shared);
}

#[test]
fn hosted_session_counts_host_frames_and_defers_resets() {
    // Wrap an emulator built elsewhere, as the desktop GUI does with its own.
    let mut session = Session::hosted(session().into_emu(), "TEST".to_owned());
    session.set_control(Control::Shared);

    session.set_human_input(Keys::START, None);
    session.run_host_frame(|emu| emu.run());
    session.step(2).unwrap();
    assert_eq!(session.status().frame, 3);
    assert_eq!(keyinput(&mut session) & 0x3FF, 0x3F7);

    session.hold(Keys::A);
    session.reset().unwrap();
    assert!(session.reset_requested());
    assert_eq!(session.status().frame, 0);
    assert_eq!(session.held(), Keys::empty());

    let session = session.rebuild(Some).unwrap();
    assert!(!session.reset_requested());
    assert_eq!(session.status().control, Control::Shared);
}
