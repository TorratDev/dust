//! Integration tests against a real `dust-headless`.
//!
//! Needs the server binary, built from the repository root (nightly workspace):
//!     cargo build --release -p dust-headless
//! or pointed to with `DUST_HEADLESS`. The test ROM is generated with `tools/fetch_test_rom.sh`
//! if missing; `DUST_TEST_ROM` selects another ROM.

use dust_client::{pb, Button, Control, InputStep, LaunchOptions, Session};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn test_rom() -> PathBuf {
    if let Some(rom) = std::env::var_os("DUST_TEST_ROM") {
        return rom.into();
    }
    let rom = repo_root().join("tests/roms/test.nds");
    if !rom.exists() {
        let status = Command::new("bash")
            .arg(repo_root().join("tools/fetch_test_rom.sh"))
            .status()
            .expect("couldn't run tools/fetch_test_rom.sh");
        assert!(status.success(), "tools/fetch_test_rom.sh failed");
    }
    rom
}

fn headless_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os(dust_client::HEADLESS_BIN_ENV) {
        return bin.into();
    }
    for profile in ["release", "debug"] {
        let bin = repo_root()
            .join("target")
            .join(profile)
            .join(format!("dust-headless{}", std::env::consts::EXE_SUFFIX));
        if bin.exists() {
            return bin;
        }
    }
    panic!(
        "dust-headless not found: run `cargo build --release -p dust-headless` in the repository \
         root, or set {}",
        dust_client::HEADLESS_BIN_ENV
    );
}

async fn launch() -> Session {
    dust_client::launch(
        test_rom(),
        &LaunchOptions {
            headless_bin: Some(headless_bin()),
            session: dust_client::SessionOptions {
                rtc_unix_time: Some(1_700_000_000),
                ..Default::default()
            },
        },
    )
    .await
    .expect("couldn't launch dust-headless")
}

#[tokio::test(flavor = "multi_thread")]
async fn boots_test_rom_for_60_cycles() {
    let session = launch().await;
    let status = session.status().await.unwrap();
    assert_eq!(status.frame, 0);
    assert!(!status.paused);
    assert_eq!(status.control, Control::Agent);
    if std::env::var_os("DUST_TEST_ROM").is_none() {
        assert_eq!(status.game_title, "DUSTTEST");
    }

    for _ in 0..60 {
        session.cycle(1).await.unwrap();
    }
    assert_eq!(session.status().await.unwrap().frame, 60);
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_resume_reset_destroy() {
    let session = launch().await;
    assert_eq!(session.cycle(10).await.unwrap().frame, 10);

    assert!(session.pause().await.unwrap().paused);
    assert!(!session.resume().await.unwrap().paused);

    let status = session.reset().await.unwrap();
    assert_eq!(status.frame, 0);
    assert_eq!(session.cycle(5).await.unwrap().frame, 5);

    // A clone keeps the launched server process alive after `destroy` consumes `session`.
    let keep_alive = session.clone();
    let id = session.id().to_owned();
    session.destroy().await.unwrap();
    let err = keep_alive
        .client()
        .grpc()
        .get_status(pb::SessionRequest { session_id: id })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_independent() {
    let first = launch().await;
    let second = first
        .client()
        .create_session(test_rom(), &Default::default())
        .await
        .unwrap();
    assert_ne!(first.id(), second.id());
    first.cycle(3).await.unwrap();
    assert_eq!(second.status().await.unwrap().frame, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn planned_rpcs_are_unimplemented() {
    let session = launch().await;
    let err = session
        .client()
        .grpc()
        .read(pb::ReadRequest {
            session_id: session.id().to_owned(),
            cpu: pb::Cpu::Arm9 as i32,
            addr: 0x0200_0000,
            len: 4,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_rom_is_invalid_argument() {
    let session = launch().await;
    let err = match session
        .client()
        .create_session("/nonexistent/rom.nds", &Default::default())
        .await
    {
        Err(dust_client::Error::Rpc(status)) => status,
        Err(err) => panic!("unexpected error: {err}"),
        Ok(_) => panic!("created a session without a ROM"),
    };
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread")]
async fn input_rpcs() {
    let session = launch().await;
    assert_eq!(
        session
            .press(&[Button::A, Button::Start], 2)
            .await
            .unwrap()
            .frame,
        2
    );
    session.hold(&[Button::B]).await.unwrap();
    session.release(&[Button::B]).await.unwrap();
    session.keypad(1 << 10).await.unwrap();
    assert_eq!(session.touch(128, 96, 3).await.unwrap().frame, 5);
    session.touch_end().await.unwrap();
    let status = session
        .sequence(&[
            InputStep {
                buttons: vec![Button::Up],
                touch: None,
                frames: 1,
            },
            InputStep {
                buttons: vec![],
                touch: Some((0, 191)),
                frames: 2,
            },
        ])
        .await
        .unwrap();
    assert_eq!(status.frame, 8);

    let err = match session.touch(256, 0, 1).await {
        Err(dust_client::Error::Rpc(status)) => status,
        other => panic!("unexpected result: {:?}", other.map(|_| ())),
    };
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
