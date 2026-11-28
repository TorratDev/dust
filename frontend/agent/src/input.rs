//! Agent input: held buttons and touch live in the [`Session`] and are applied as a full set
//! before every emulated frame, bypassing the keyboard and the keymap.

use crate::{Error, Session, StepResult};
pub use dust_core::emu::input::Keys;

/// The lid switch (EXTKEYIN bit 7, bit 23 of the combined status). Unlike buttons, its bit is set
/// while the lid is closed.
pub const LID_CLOSED: Keys = Keys::from_bits_retain(1 << 23);

/// Touchscreen size in pixels.
pub const TOUCH_WIDTH: u16 = 256;
pub const TOUCH_HEIGHT: u16 = 192;

/// Converts a raw keypad mask in py-desmume's bit order (`1 << (KEY_x - 1)`: A, B, Select,
/// Start, Right, Left, Up, Down, R, L, X, Y, Debug, Boost, Lid) to dust's keys. Boost has no DS
/// equivalent and is ignored.
pub fn keys_from_desmume_mask(mask: u32) -> Keys {
    // Bits 0-9 (A to L) already match dust's layout.
    let mut keys = Keys::from_bits_truncate(mask & 0x3FF);
    for (bit, key) in [
        (10, Keys::X),
        (11, Keys::Y),
        (12, Keys::DEBUG),
        (14, LID_CLOSED),
    ] {
        if mask & 1 << bit != 0 {
            keys |= key;
        }
    }
    keys
}

/// One step of [`Session::sequence`]: hold exactly `keys` (and touch `touch`, in pixels) for
/// `frames` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputStep {
    pub keys: Keys,
    pub touch: Option<[u16; 2]>,
    pub frames: u32,
}

fn check_touch(x: u16, y: u16) -> Result<(), Error> {
    if x >= TOUCH_WIDTH || y >= TOUCH_HEIGHT {
        return Err(Error::InvalidTouch { x, y });
    }
    Ok(())
}

impl Session {
    /// Buttons held by the agent, applied from the next emulated frame on.
    pub fn held(&self) -> Keys {
        self.keys
    }

    pub fn set_held(&mut self, keys: Keys) {
        self.keys = keys;
    }

    pub fn hold(&mut self, keys: Keys) {
        self.keys |= keys;
    }

    pub fn release(&mut self, keys: Keys) {
        // Not `&= !keys`: `!` drops bits outside `Keys`, such as the lid.
        self.keys.remove(keys);
    }

    /// Replaces the held buttons with a raw mask in py-desmume's bit order.
    pub fn set_keypad(&mut self, mask: u32) {
        self.keys = keys_from_desmume_mask(mask);
    }

    /// Holds `keys` for `frames` frames, then releases them.
    pub fn press(&mut self, keys: Keys, frames: u32) -> Result<StepResult, Error> {
        self.hold(keys);
        let result = self.step(frames);
        self.release(keys);
        result
    }

    /// Touches pixel (`x`, `y`) of the bottom screen from the next frame on, until
    /// [`Session::touch_end`].
    pub fn touch_start(&mut self, x: u16, y: u16) -> Result<(), Error> {
        check_touch(x, y)?;
        self.touch = Some([x, y]);
        Ok(())
    }

    pub fn touch_end(&mut self) {
        self.touch = None;
    }

    /// Touches (`x`, `y`) for `frames` frames, then lifts the pen. With `frames` = 0 the pen stays
    /// down until [`Session::touch_end`] (the API has no separate touch-down call).
    pub fn touch(&mut self, x: u16, y: u16, frames: u32) -> Result<StepResult, Error> {
        self.touch_start(x, y)?;
        if frames == 0 {
            return Ok(StepResult {
                status: self.status(),
                changes: Vec::new(),
                hits: Vec::new(),
            });
        }
        let result = self.step(frames);
        self.touch_end();
        result
    }

    /// Runs `steps` in order; afterwards the held buttons and touch are back to what they were.
    /// The watch changes and hook hits of every step are returned together.
    pub fn sequence(&mut self, steps: &[InputStep]) -> Result<StepResult, Error> {
        for step in steps {
            if let Some([x, y]) = step.touch {
                check_touch(x, y)?;
            }
        }
        let (prev_keys, prev_touch) = (self.keys, self.touch);
        let (mut changes, mut hits) = (Vec::new(), Vec::new());
        let mut result = Ok(());
        for step in steps {
            self.keys = step.keys;
            self.touch = step.touch;
            match self.step(step.frames) {
                Ok(step_result) => {
                    changes.extend(step_result.changes);
                    hits.extend(step_result.hits);
                }
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        }
        (self.keys, self.touch) = (prev_keys, prev_touch);
        result.map(|()| StepResult {
            status: self.status(),
            changes,
            hits,
        })
    }

    /// Makes the emulated input match the agent's: presses held buttons, releases the rest.
    pub(crate) fn apply_input(&mut self) {
        // Press before releasing, so no key IRQ sees a transient all-released state.
        self.emu.press_keys(self.keys & Keys::all());
        self.emu.release_keys(Keys::all() - self.keys);
        // The lid bit is set when closed, so "releasing" it closes the lid.
        if self.keys.contains(LID_CLOSED) {
            self.emu.release_keys(LID_CLOSED);
        } else {
            self.emu.press_keys(LID_CLOSED);
        }
        match self.touch {
            // The touchscreen controller reports 12-bit positions: 16 units per pixel.
            Some([x, y]) => self.emu.set_touch_pos([x * 16, y * 16]),
            None => self.emu.end_touch(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desmume_mask() {
        assert_eq!(keys_from_desmume_mask(0), Keys::empty());
        assert_eq!(keys_from_desmume_mask(1), Keys::A);
        assert_eq!(keys_from_desmume_mask(1 << 9), Keys::L);
        assert_eq!(keys_from_desmume_mask(1 << 10 | 1 << 11), Keys::X | Keys::Y);
        assert_eq!(keys_from_desmume_mask(1 << 12), Keys::DEBUG);
        assert_eq!(keys_from_desmume_mask(1 << 13), Keys::empty());
        assert_eq!(keys_from_desmume_mask(1 << 14), LID_CLOSED);
    }
}
