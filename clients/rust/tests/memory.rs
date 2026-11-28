//! Memory and CPU RPCs against a real `dust-headless` (see `boot.rs` for what it needs).

mod common;

use dust_client::{Cpu, MemRange};

#[tokio::test(flavor = "multi_thread")]
async fn main_ram_round_trip() {
    let session = common::launch().await;
    session
        .write(Cpu::Arm9, 0x0220_0000, &[1, 2, 3, 4, 5])
        .await
        .unwrap();
    assert_eq!(
        session.read(Cpu::Arm9, 0x0220_0000, 5).await.unwrap(),
        [1, 2, 3, 4, 5]
    );
    session
        .write_u32(Cpu::Arm9, 0x0220_0010, 0xDEAD_BEEF)
        .await
        .unwrap();
    assert_eq!(
        session.read_u32(Cpu::Arm9, 0x0220_0010).await.unwrap(),
        0xDEAD_BEEF
    );
    // Main RAM is mirrored, and the ARM7 sees the same memory.
    assert_eq!(
        session.read_u16(Cpu::Arm7, 0x0260_0010).await.unwrap(),
        0xBEEF
    );
    let many = session
        .read_many(
            Cpu::Arm9,
            &[
                MemRange {
                    addr: 0x0220_0000,
                    len: 2,
                },
                MemRange {
                    addr: 0x0220_0013,
                    len: 1,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(many, [vec![1, 2], vec![0xDE]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn shared_wram_round_trip() {
    let session = common::launch().await;
    // After direct boot all shared WRAM is mapped to the ARM7 at 0x0300_0000.
    session
        .write_u16(Cpu::Arm7, 0x0300_0100, 0x1234)
        .await
        .unwrap();
    session
        .write(Cpu::Arm7, 0x0300_0102, &[0x56])
        .await
        .unwrap();
    assert_eq!(
        session.read(Cpu::Arm7, 0x0300_0100, 3).await.unwrap(),
        [0x34, 0x12, 0x56]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn regs_and_set_reg() {
    let session = common::launch().await;
    session.cycle(1).await.unwrap();
    for cpu in [Cpu::Arm9, Cpu::Arm7] {
        let regs = session.regs(cpu).await.unwrap();
        assert_eq!(regs.gprs.len(), 16);
        assert_ne!(regs.cpsr, 0);

        session.set_reg(cpu, 0, 0x1234_5678).await.unwrap();
        let after = session.regs(cpu).await.unwrap();
        assert_eq!(after.gprs[0], 0x1234_5678);
        assert_eq!(after.gprs[1..], regs.gprs[1..]);
    }
    let err = session.set_reg(Cpu::Arm9, 17, 0).await.unwrap_err();
    assert!(
        matches!(err, dust_client::Error::Rpc(status) if status.code() == tonic::Code::InvalidArgument)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reports_write() {
    let session = common::launch().await;
    let id = session.watch(Cpu::Arm9, 0x0220_0100, 4).await.unwrap();
    let old = session.read(Cpu::Arm9, 0x0220_0100, 4).await.unwrap();
    let new = old.iter().map(|byte| !byte).collect::<Vec<_>>();
    session.write(Cpu::Arm9, 0x0220_0100, &new).await.unwrap();

    let (_, changes) = session.step(1).await.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].watch_id, id);
    assert_eq!(changes[0].addr, 0x0220_0100);
    assert_eq!(changes[0].old_data, old);
    assert_eq!(changes[0].new_data, new);

    // Reported once, then silent until the next change.
    assert!(session.step(1).await.unwrap().1.is_empty());
    session.unwatch(id).await.unwrap();
    session.write(Cpu::Arm9, 0x0220_0100, &old).await.unwrap();
    assert!(session.step(1).await.unwrap().1.is_empty());
}
