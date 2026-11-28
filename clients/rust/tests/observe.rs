//! Observability RPCs against a real `dust-headless` (see `boot.rs` for what it needs).

mod common;

use dust_client::{pb::event::Kind, Cpu, LogLevel};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn memory_map_and_dump() {
    let session = common::launch().await;
    session
        .write(Cpu::Arm9, 0x0210_0000, &[9, 8, 7])
        .await
        .unwrap();
    let map = session.memory_map().await.unwrap();
    let main_ram = map.iter().find(|region| region.name == "main_ram").unwrap();
    assert_eq!((main_ram.start, main_ram.size), (0x0200_0000, 4 << 20));
    assert!(map.iter().any(|region| region.name == "vram_a"));
    assert!(map
        .iter()
        .any(|region| region.name == "arm7_io" && region.cpu() == Cpu::Arm7));

    let dump = session.dump("main_ram").await.unwrap();
    assert_eq!(dump.len(), 4 << 20);
    assert_eq!(dump[0x10_0000..0x10_0003], [9, 8, 7]);
    let err = session.dump("nope").await.unwrap_err();
    assert!(
        matches!(err, dust_client::Error::Rpc(status) if status.code() == tonic::Code::InvalidArgument)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disasm_at_rom_entry() {
    let session = common::launch().await;
    let rom = std::fs::read(common::test_rom()).unwrap();
    let entry = u32::from_le_bytes(rom[0x24..0x28].try_into().unwrap());
    let instrs = session.disasm(Cpu::Arm9, entry, 8, false).await.unwrap();
    assert_eq!(instrs.len(), 8);
    for (i, instr) in instrs.iter().enumerate() {
        let addr = entry + i as u32 * 4;
        assert_eq!(instr.addr, addr);
        assert_eq!(instr.raw, session.read_u32(Cpu::Arm9, addr).await.unwrap());
        assert!(!instr.text.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_while_running() {
    let session = common::launch().await;
    session.cycle(30).await.unwrap();
    let stats = session.stats(0).await.unwrap();
    assert!(stats.fps > 0.0, "{stats:?}");
    assert!(stats.frame_time_avg_us > 0.0);
    assert_eq!(stats.total_frames, 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn unmapped_io_write_is_logged() {
    const CODE_ADDR: u32 = 0x0230_0000;
    let session = common::launch().await;
    let mut events = session.events().await.unwrap();

    // Make the ARM9 run `str r1, [0x0400_0FF0]` (an unmapped I/O port), then spin.
    let code: [u32; 4] = [0xE3A0_0301, 0xE380_0EFF, 0xE580_1000, 0xEAFF_FFFE];
    let bytes = code
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect::<Vec<_>>();
    session.write(Cpu::Arm9, CODE_ADDR, &bytes).await.unwrap();
    session.set_reg(Cpu::Arm9, 16, 0xDF).await.unwrap();
    session.set_reg(Cpu::Arm9, 1, 0xCAFE).await.unwrap();
    session.set_reg(Cpu::Arm9, 15, CODE_ADDR).await.unwrap();
    session.cycle(1).await.unwrap();

    const MESSAGE: &str = "Unknown IO write32 @ 0xFF0: 0x0000CAFE";
    let logs = session.logs(0, LogLevel::Warning).await.unwrap();
    let entry = logs
        .iter()
        .find(|entry| entry.message == MESSAGE)
        .unwrap_or_else(|| panic!("not logged: {logs:#?}"));
    assert_eq!(entry.level(), LogLevel::Warning);
    assert_eq!(entry.frame, 0);
    assert_eq!(entry.fields.get("cpu").map(String::as_str), Some("arm9"));

    // The same entry arrives on the event stream.
    let event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.message().await.unwrap().unwrap();
            if let Some(Kind::Log(log)) = event.kind {
                if log.message == MESSAGE {
                    return log;
                }
            }
        }
    })
    .await
    .expect("no log event");
    assert_eq!(event.seq, entry.seq);
}
