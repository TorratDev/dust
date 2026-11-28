//! Screenshots, save states and save files. Uses the ROM from `tools/fetch_test_rom.sh`
//! (generated if missing) or `$DUST_TEST_ROM`.

use dust_agent::{Cpu, ImageFormat, SaveFormat, Session, SessionOptions};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
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

fn ram_hash(session: &mut Session) -> u64 {
    let ram = session.read(Cpu::Arm9, 0x0200_0000, 4 << 20).unwrap();
    let mut hasher = DefaultHasher::new();
    ram.hash(&mut hasher);
    session.regs(Cpu::Arm9).hash(&mut hasher);
    hasher.finish()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("agent-states");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn state_bytes_restore_frame_and_ram() {
    let mut session = session();
    session.step(60).unwrap();
    let (frame, hash) = (session.status().frame, ram_hash(&mut session));
    let state = session.save_state_bytes(false).unwrap();

    session.step(120).unwrap();
    let after_hash = ram_hash(&mut session);
    assert_eq!(session.status().frame, frame + 120);

    session.load_state_bytes(&state.state, None).unwrap();
    assert_eq!(session.status().frame, frame);
    assert_eq!(ram_hash(&mut session), hash);

    // Emulation from a loaded state is deterministic.
    session.step(120).unwrap();
    assert_eq!(ram_hash(&mut session), after_hash);
}

#[test]
fn state_file_restores_frame_and_ram() {
    let path = scratch("state_file.state");
    let mut session = session();
    session.step(60).unwrap();
    let (frame, hash) = (session.status().frame, ram_hash(&mut session));
    let screen = session.screenshot(ImageFormat::RawRgb).unwrap();
    session.save_state(&path, true).unwrap();

    session.step(120).unwrap();
    session.load_state(&path).unwrap();
    assert_eq!(session.status().frame, frame);
    assert_eq!(ram_hash(&mut session), hash);
    assert_eq!(session.screenshot(ImageFormat::RawRgb).unwrap(), screen);

    // Another session of the same ROM loads it too.
    let mut other = self::session();
    other.load_state(&path).unwrap();
    assert_eq!(other.status().frame, frame);
    assert_eq!(ram_hash(&mut other), hash);
}

#[test]
fn bad_state_leaves_session_unchanged() {
    let mut session = session();
    session.step(10).unwrap();
    let hash = ram_hash(&mut session);
    assert!(session.load_state_bytes(&[0; 16], None).is_err());
    let mut state = session.save_state_bytes(false).unwrap().state;
    state[..64].fill(0xFF);
    assert!(session.load_state_bytes(&state, None).is_err());
    assert_eq!(session.status().frame, 10);
    assert_eq!(ram_hash(&mut session), hash);
}

#[test]
fn screenshot_sizes() {
    let mut session = session();
    session.step(30).unwrap();

    let raw = session.screenshot(ImageFormat::RawRgb).unwrap();
    assert_eq!((raw.width, raw.height), (256, 384));
    assert_eq!(raw.data.len(), 256 * 384 * 3);

    let png = session.screenshot(ImageFormat::Png).unwrap();
    assert_eq!((png.width, png.height), (256, 384));
    let decoder = png::Decoder::new(&png.data[..]);
    let mut reader = decoder.read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut pixels).unwrap();
    assert_eq!((info.width, info.height), (256, 384));
    assert_eq!(&pixels[..info.buffer_size()], &raw.data[..]);
}

#[test]
fn save_files_round_trip() {
    let mut session = session();
    assert!(session.save_data().is_none());
    assert!(session
        .export_save(scratch("none.sav"), SaveFormat::Raw)
        .is_err());

    // A 64 KiB EEPROM save, imported from a DeSmuME .dsv.
    let raw: Vec<u8> = (0..0x1_0000u32).map(|i| (i * 7) as u8).collect();
    let sav = scratch("save.sav");
    let dsv = scratch("save.dsv");
    std::fs::write(&sav, &raw).unwrap();
    session.import_save(&sav).unwrap();
    session.export_save(&dsv, SaveFormat::Dsv).unwrap();
    assert_eq!(std::fs::read(&dsv).unwrap().len(), raw.len() + 122);

    let mut other = self::session();
    other.import_save(&dsv).unwrap();
    assert_eq!(other.save_data().unwrap(), raw);
    let exported = scratch("exported.sav");
    other.export_save(&exported, SaveFormat::Raw).unwrap();
    assert_eq!(std::fs::read(&exported).unwrap(), raw);

    // The imported save survives a reset and is stored in states.
    other.reset().unwrap();
    assert_eq!(other.save_data().unwrap(), raw);
    let state = other.save_state_bytes(true).unwrap();
    assert_eq!(state.save.as_deref(), Some(&raw[..]));

    // A state with a save gives a save-less session that save.
    let mut fresh = self::session();
    fresh
        .load_state_bytes(&state.state, state.save.as_deref())
        .unwrap();
    assert_eq!(fresh.save_data().unwrap(), raw);

    // Unsupported sizes are rejected.
    let odd = scratch("odd.sav");
    std::fs::write(&odd, vec![0; 3]).unwrap();
    assert!(session.import_save(&odd).is_err());
}

#[test]
fn save_option_loads_save() {
    let sav = scratch("option.sav");
    std::fs::write(&sav, vec![0x5A; 0x200]).unwrap();
    let session = Session::new(
        test_rom(),
        &SessionOptions {
            save: Some(sav),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(session.save_data().unwrap(), vec![0x5A; 0x200]);
}
