//! Screenshot, save state and save file RPCs against a real `dust-headless` (see `boot.rs` for
//! what it needs).

mod common;

use dust_client::{Cpu, ImageFormat, SaveFormat};
use std::path::PathBuf;

async fn ram(session: &dust_client::Session) -> Vec<u8> {
    session.read(Cpu::Arm9, 0x0200_0000, 4 << 20).await.unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("client-states");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshot_has_both_screens() {
    let session = common::launch().await;
    session.cycle(30).await.unwrap();
    let raw = session.screenshot(ImageFormat::RawRgb).await.unwrap();
    assert_eq!((raw.width, raw.height), (256, 384));
    assert_eq!(raw.format(), ImageFormat::RawRgb);
    assert_eq!(raw.data.len(), 256 * 384 * 3);
    let png = session.screenshot(ImageFormat::Png).await.unwrap();
    assert_eq!((png.width, png.height), (256, 384));
    assert!(png.data.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn save_step_load_restores_frame_and_ram() {
    let session = common::launch().await;
    session.cycle(60).await.unwrap();
    let ram_before = ram(&session).await;

    let state = session.save_state_bytes(false).await.unwrap();
    let path = scratch("saved.state");
    session.save_state(&path, false).await.unwrap();

    assert_eq!(session.cycle(120).await.unwrap().frame, 180);
    assert_eq!(session.load_state_bytes(&state).await.unwrap().frame, 60);
    assert_eq!(ram(&session).await, ram_before);

    session.cycle(120).await.unwrap();
    assert_eq!(session.load_state(&path).await.unwrap().frame, 60);
    assert_eq!(ram(&session).await, ram_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn save_files_round_trip() {
    let session = common::launch().await;
    let err = session
        .export_save(scratch("none.sav"), SaveFormat::Raw)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, dust_client::Error::Rpc(status) if status.code() == tonic::Code::FailedPrecondition),
        "{err}"
    );

    let raw: Vec<u8> = (0..0x200u32).map(|i| i as u8).collect();
    let sav = scratch("in.sav");
    std::fs::write(&sav, &raw).unwrap();
    session.import_save(&sav).await.unwrap();
    let dsv = scratch("out.dsv");
    session.export_save(&dsv, SaveFormat::Dsv).await.unwrap();
    session.import_save(&dsv).await.unwrap();
    let out = scratch("out.sav");
    session.export_save(&out, SaveFormat::Raw).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), raw);
}
