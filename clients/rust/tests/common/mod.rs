//! Helpers for the integration tests against a real `dust-headless`.
//!
//! Needs the server binary, built from the repository root:
//!     cargo build --release -p dust-headless
//! or pointed to with `DUST_HEADLESS`. The test ROM is generated with `tools/fetch_test_rom.sh`
//! (and regenerated when outdated); `DUST_TEST_ROM` selects another ROM.

#![allow(dead_code)]

use dust_client::{LaunchOptions, Session};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn test_rom() -> PathBuf {
    if let Some(rom) = std::env::var_os("DUST_TEST_ROM") {
        return rom.into();
    }
    // The script returns right away when the ROM is up to date.
    let output = Command::new("bash")
        .arg(repo_root().join("tools/fetch_test_rom.sh"))
        .output()
        .expect("couldn't run tools/fetch_test_rom.sh");
    assert!(
        output.status.success(),
        "tools/fetch_test_rom.sh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    repo_root().join("tests/roms/test.nds")
}

pub fn headless_bin() -> PathBuf {
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

pub async fn launch() -> Session {
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
