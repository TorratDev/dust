//! Memory map, disassembly, stats and logs. Uses the ROM from `tools/fetch_test_rom.sh`
//! (generated if missing) or `$DUST_TEST_ROM`.

use dust_agent::{memory, Cpu, LogLevel, Session, SessionOptions};
use dust_core::cpu::disasm::disassemble_range;
use std::{
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

fn arm9_entry() -> u32 {
    let rom = std::fs::read(test_rom()).unwrap();
    u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap())
}

/// Makes the ARM9 run `str r1, [0x0400_0FF0]` (an unmapped I/O port), then spin.
fn store_to_unmapped_io(session: &mut Session) {
    const CODE_ADDR: u32 = 0x0230_0000;
    let code: [u32; 4] = [
        0xE3A0_0301, // mov r0, #0x0400_0000
        0xE380_0EFF, // orr r0, r0, #0xFF0
        0xE580_1000, // str r1, [r0]
        0xEAFF_FFFE, // b .
    ];
    let bytes = code
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect::<Vec<_>>();
    session.write(Cpu::Arm9, CODE_ADDR, &bytes).unwrap();
    // System mode, ARM state, IRQs and FIQs disabled.
    session.set_reg(Cpu::Arm9, memory::REG_CPSR, 0xDF).unwrap();
    session.set_reg(Cpu::Arm9, 1, 0x1234_5678).unwrap();
    session
        .set_reg(Cpu::Arm9, memory::REG_PC, CODE_ADDR)
        .unwrap();
}

#[test]
fn memory_map_and_dump() {
    let mut session = session();
    session.write(Cpu::Arm9, 0x0210_0000, &[9, 8, 7]).unwrap();
    let map = session.memory_map();
    for name in [
        "main_ram",
        "shared_wram",
        "arm7_wram",
        "vram_a",
        "vram_i",
        "palettes",
        "oam",
        "arm9_io",
        "arm7_io",
    ] {
        let region = map.iter().find(|region| region.name == name).unwrap();
        assert_eq!(
            session.dump(name).unwrap().len(),
            region.size as usize,
            "{name}"
        );
    }
    let main_ram = session.dump("main_ram").unwrap();
    assert_eq!(main_ram.len(), 4 << 20);
    assert_eq!(main_ram[0x10_0000..0x10_0003], [9, 8, 7]);

    // I/O dumps match bus reads: KEYINPUT reads as all released.
    let io = session.dump("arm9_io").unwrap();
    assert_eq!(io[0x130..0x132], [0xFF, 0x03]);

    assert!(matches!(
        session.dump("nope"),
        Err(dust_agent::Error::InvalidArgument(_))
    ));
}

#[test]
fn disasm_matches_gui_view() {
    let mut session = session();
    let entry = arm9_entry();
    let instrs = session.disasm(Cpu::Arm9, entry, 16, false).unwrap();
    assert_eq!(instrs.len(), 16);

    // What the desktop GUI's disassembly view shows for the same range.
    let mut gui = Vec::new();
    disassemble_range::<_, true>(session.emu(), (entry, entry + 15 * 4), false, &mut gui);
    for (i, (instr, gui)) in instrs.iter().zip(&gui).enumerate() {
        assert_eq!(instr.addr, entry + i as u32 * 4);
        assert_eq!(instr.addr, gui.addr);
        assert_eq!(instr.raw, gui.raw);
        let gui_text = if gui.comment.is_empty() {
            gui.opcode.clone()
        } else {
            format!("{} ; {}", gui.opcode, gui.comment)
        };
        assert_eq!(instr.text, gui_text);
        assert!(!instr.text.is_empty());
    }
    let word = session.read(Cpu::Arm9, entry, 4).unwrap();
    assert_eq!(instrs[0].raw, u32::from_le_bytes(word.try_into().unwrap()));

    let thumb = session.disasm(Cpu::Arm9, entry + 1, 3, true).unwrap();
    assert_eq!(
        thumb.iter().map(|instr| instr.addr).collect::<Vec<_>>(),
        [entry, entry + 2, entry + 4]
    );
    assert!(session
        .disasm(Cpu::Arm7, entry, 0, false)
        .unwrap()
        .is_empty());
    assert!(session.disasm(Cpu::Arm9, entry, 1 << 20, false).is_err());
}

#[test]
fn stats_while_running() {
    let mut session = session();
    assert_eq!(session.stats(0).fps, 0.0);
    session.step(30).unwrap();
    let stats = session.stats(0);
    assert!(stats.fps > 0.0, "{stats:?}");
    assert!(stats.speed_percent > 0.0);
    assert!(stats.frame_time_avg_us > 0.0);
    assert!(stats.frame_time_max_us >= stats.frame_time_avg_us);
    assert_eq!(stats.total_frames, 30);
    assert!(stats.late_frames <= 30);
    // The test ROM draws no 3D.
    assert_eq!((stats.polygons, stats.vertices), (0, 0));

    session.reset().unwrap();
    session.step(1).unwrap();
    assert_eq!(session.stats(0).total_frames, 31);
}

#[test]
fn unmapped_io_write_is_logged() {
    let mut session = session();
    session.step(2).unwrap();
    // Debug accesses (the agent's own reads and writes) aren't logged.
    session.write_value(Cpu::Arm9, 0x0400_0FF0, 1, 4).unwrap();
    assert!(session
        .logs(0, LogLevel::Trace)
        .iter()
        .all(|entry| !entry.message.contains("Unknown IO write32 @ 0xFF0")));

    store_to_unmapped_io(&mut session);
    session.step(1).unwrap();
    let logs = session.logs(0, LogLevel::Warning);
    let entry = logs
        .iter()
        .find(|entry| entry.message == "Unknown IO write32 @ 0xFF0: 0x12345678")
        .unwrap_or_else(|| panic!("not logged: {logs:#?}"));
    assert_eq!(entry.level, LogLevel::Warning);
    assert_eq!(entry.frame, 2);
    assert_eq!(entry.fields.get("cpu").map(String::as_str), Some("arm9"));

    // `since` and `min_level` filter.
    assert!(session
        .logs(entry.seq, LogLevel::Trace)
        .iter()
        .all(|later| later.seq > entry.seq));
    assert!(session.logs(0, LogLevel::Error).is_empty());
}
