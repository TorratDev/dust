//! Uses the ROM from `tools/fetch_test_rom.sh` (generated, or regenerated if outdated) or
//! `$DUST_TEST_ROM`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn test_rom() -> PathBuf {
    if let Some(rom) = std::env::var_os("DUST_TEST_ROM") {
        return rom.into();
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // The script returns right away when the ROM is up to date.
    let output = Command::new("bash")
        .arg(root.join("tools/fetch_test_rom.sh"))
        .output()
        .expect("couldn't run tools/fetch_test_rom.sh");
    assert!(
        output.status.success(),
        "tools/fetch_test_rom.sh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    root.join("tests/roms/test.nds")
}
