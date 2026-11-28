//! Measures total emulated frames per second of `StepBatch` with 1, 4, 8 and 16 sessions on one
//! `dust-headless`.
//!
//! Usage: DUST_HEADLESS=<dust-headless> cargo run --release --example batch_bench -- <rom> [frames]
//! (`frames`: frames each session emulates per size, default 600, in batches of 10).

use dust_client::{Action, Batch, LaunchOptions, SessionOptions};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), dust_client::Error> {
    let mut args = std::env::args().skip(1);
    let rom = args.next().expect("usage: batch_bench <rom> [frames]");
    let frames: u32 = args
        .next()
        .map_or(600, |n| n.parse().expect("invalid frame count"));
    let opts = LaunchOptions {
        session: SessionOptions {
            rtc_unix_time: Some(1_700_000_000),
            ..Default::default()
        },
        ..Default::default()
    };
    println!("| Sessions | Frames/s (total) | Frames/s (per session) |");
    println!("|---:|---:|---:|");
    for count in [1, 4, 8, 16] {
        let batch = Batch::launch(&rom, &opts, count).await?;
        let action = Action::default();
        // Warm up.
        batch.step_all(&action, 10).await?;
        let start = Instant::now();
        for _ in 0..frames / 10 {
            batch.step_all(&action, 10).await?;
        }
        let total = (frames / 10 * 10) as f64 * count as f64 / start.elapsed().as_secs_f64();
        println!("| {count} | {total:.0} | {:.0} |", total / count as f64);
    }
    Ok(())
}
