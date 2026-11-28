//! `dust-agent`: drive dust's emulator core from programs.
//!
//! A [`Session`] owns one emulator instance (soft sync 2D + soft 3D renderers, no audio output)
//! and advances it only when asked to. `Session` is not `Send`; servers create and use each one on
//! its own thread (see `dust-agent-grpc`).

mod input;
pub mod memory;
mod rtc;
mod session;
mod soft_renderer_3d;

pub use input::{keys_from_desmume_mask, InputStep, Keys, LID_CLOSED, TOUCH_HEIGHT, TOUCH_WIDTH};
pub use memory::{Cpu, Regs, WatchChange};
pub use session::{Control, Error, Session, SessionOptions, Status, StepResult};
