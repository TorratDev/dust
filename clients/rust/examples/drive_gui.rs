//! Drives the desktop GUI's own session: start the GUI with the agent server on, e.g.
//!
//! ```text
//! cargo run -p dust-desktop --features agent-server -- --agent-port 50051 tests/roms/test.nds
//! ```
//!
//! then run `cargo run --example drive_gui -- [ENDPOINT] [SCREENSHOT.png]` here (defaults:
//! `http://127.0.0.1:50051`, `gui.png`). It presses A for a few frames, reports the buttons the
//! game sees (keyboard presses show up too in shared control) and saves a screenshot.

use dust_client::{Button, Client, Cpu, ImageFormat, GUI_SESSION_ID};

const KEYINPUT: u32 = 0x0400_0130;

/// Names of the buttons pressed in a KEYINPUT value (bits are 0 while pressed).
fn pressed(keyinput: u16) -> Vec<&'static str> {
    const NAMES: [&str; 10] = [
        "A", "B", "Select", "Start", "Right", "Left", "Up", "Down", "R", "L",
    ];
    NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| keyinput & 1 << bit == 0)
        .map(|(_, name)| *name)
        .collect()
}

#[tokio::main]
async fn main() -> dust_client::Result<()> {
    let mut args = std::env::args().skip(1);
    let endpoint = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_owned());
    let screenshot_path = args.next().unwrap_or_else(|| "gui.png".to_owned());

    let session = Client::connect(endpoint).await?.session(GUI_SESSION_ID);
    let status = session.status().await?;
    println!(
        "connected: {:?} at frame {}, control {:?}",
        status.game_title, status.frame, status.control
    );

    // KEYINPUT is latched by hardware at once, so reading it right after the press shows it.
    session.hold(&[Button::A]).await?;
    session.cycle(5).await?;
    let keys = session.read_u16(Cpu::Arm9, KEYINPUT).await?;
    println!("while the agent holds A: pressed {:?}", pressed(keys));
    session.release(&[Button::A]).await?;
    session.cycle(1).await?;
    let keys = session.read_u16(Cpu::Arm9, KEYINPUT).await?;
    println!("after release: pressed {:?}", pressed(keys));

    let image = session.screenshot(ImageFormat::Png).await?;
    std::fs::write(&screenshot_path, &image.data)?;
    println!(
        "screenshot: {}x{} -> {screenshot_path}",
        image.width, image.height
    );
    Ok(())
}
