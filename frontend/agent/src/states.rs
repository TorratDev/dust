//! Save states and cartridge save files.
//!
//! State files use the desktop savestate editor's layout (`frontend/desktop/src/ui/savestate_editor.rs`),
//! deflate-compressed:
//!
//! ```text
//! persistent savestate | [frame: u64 LE] | [cartridge save] | framebuffer (2 x 256x192 u32) | info: u32 LE
//! ```
//!
//! `info` holds the save length (bits 0-27), whether the frame counter is present (bit 29, written by
//! dust-agent and ignored by the GUI, which sees it as trailing persistent data), whether the
//! framebuffer is little-endian (bit 30) and whether a save is present (bit 31). The GUI and agents
//! can load each other's states; states from the GUI keep the session's frame counter.
//!
//! In-memory states ([`Session::save_state_bytes`]) use the same layout, uncompressed and without the
//! save, which is returned separately.

use crate::{Error, Session};
use dust_core::{
    ds_slot::spi::{eeprom_4k::Eeprom4k, eeprom_fram::EepromFram, flash::Flash, Spi},
    gpu::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH},
    utils::{
        BoxedByteSlice, PersistentReadSavestate, PersistentWriteSavestate, ReadSavestate,
        WriteSavestate,
    },
    SaveContents, SaveReloadContents,
};
use miniz_oxide::{
    deflate::{compress_to_vec, CompressionLevel},
    inflate::decompress_to_vec,
};
use std::{fs, path::Path};

const FRAMEBUFFER_LEN: usize = 2 * 4 * SCREEN_WIDTH * SCREEN_HEIGHT;
const INFO_SAVE_LEN_MASK: u32 = 0x0FFF_FFFF;
const INFO_HAS_FRAME: u32 = 1 << 29;
const INFO_FB_IS_LE: u32 = 1 << 30;
const INFO_HAS_SAVE: u32 = 1 << 31;

/// An in-memory save state, see [`Session::save_state_bytes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateBytes {
    pub state: Vec<u8>,
    /// The cartridge save, when requested.
    pub save: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SaveFormat {
    /// The raw save memory (`.sav`).
    #[default]
    Raw,
    /// DeSmuME's `.dsv`: the raw save followed by a footer.
    Dsv,
}

fn invalid_state(msg: impl Into<String>) -> Error {
    Error::InvalidState(msg.into())
}

struct Decoded<'a> {
    contents: &'a [u8],
    frame: Option<u64>,
    save: Option<&'a [u8]>,
    framebuffer: Box<Framebuffer>,
}

fn encode(contents: &[u8], frame: u64, save: Option<&[u8]>, framebuffer: &Framebuffer) -> Vec<u8> {
    let save_len = save.map_or(0, <[u8]>::len);
    let mut data = Vec::with_capacity(contents.len() + 8 + save_len + FRAMEBUFFER_LEN + 4);
    data.extend_from_slice(contents);
    data.extend_from_slice(&frame.to_le_bytes());
    let mut info = INFO_HAS_FRAME | INFO_FB_IS_LE;
    if let Some(save) = save {
        data.extend_from_slice(save);
        info |= INFO_HAS_SAVE | save.len() as u32;
    }
    for pixel in framebuffer.iter().flatten() {
        data.extend_from_slice(&pixel.to_le_bytes());
    }
    data.extend_from_slice(&info.to_le_bytes());
    data
}

fn split_end(data: &[u8], len: usize) -> Result<(&[u8], &[u8]), Error> {
    let pos = data
        .len()
        .checked_sub(len)
        .ok_or_else(|| invalid_state("truncated state"))?;
    Ok(data.split_at(pos))
}

fn decode(data: &[u8]) -> Result<Decoded<'_>, Error> {
    let (data, info) = split_end(data, 4)?;
    let info = u32::from_le_bytes(info.try_into().unwrap());

    let (data, fb_bytes) = split_end(data, FRAMEBUFFER_LEN)?;
    let mut framebuffer = Box::new([[0; SCREEN_WIDTH * SCREEN_HEIGHT]; 2]);
    for (pixel, bytes) in framebuffer
        .iter_mut()
        .flatten()
        .zip(fb_bytes.chunks_exact(4))
    {
        let bytes = bytes.try_into().unwrap();
        *pixel = if info & INFO_FB_IS_LE != 0 {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        };
    }

    let (data, save) = if info & INFO_HAS_SAVE != 0 {
        let (data, save) = split_end(data, (info & INFO_SAVE_LEN_MASK) as usize)?;
        (data, Some(save))
    } else {
        (data, None)
    };

    let (contents, frame) = if info & INFO_HAS_FRAME != 0 {
        let (contents, frame) = split_end(data, 8)?;
        (
            contents,
            Some(u64::from_le_bytes(frame.try_into().unwrap())),
        )
    } else {
        (data, None)
    };

    Ok(Decoded {
        contents,
        frame,
        save,
        framebuffer,
    })
}

fn read_file(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|err| Error::Io {
        path: path.to_owned(),
        err,
    })
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), Error> {
    fs::write(path, data).map_err(|err| Error::Io {
        path: path.to_owned(),
        err,
    })
}

/// Builds the cartridge SPI device for a save of `len` bytes (dust's save types, by size).
pub(crate) fn spi_for_save(contents: SaveContents, logger: &slog::Logger) -> Result<Spi, Error> {
    let len = contents.len();
    let invalid = || Error::InvalidSave(format!("unsupported save size: {len} B"));
    let logger = logger.new(slog::o!("ds_spi" => ""));
    Ok(match len {
        0x200 => Eeprom4k::new(contents, None, logger)
            .map_err(|_| invalid())?
            .into(),
        0x2000 | 0x1_0000 | 0x2_0000 => EepromFram::new(contents, None, logger)
            .map_err(|_| invalid())?
            .into(),
        0x4_0000 | 0x8_0000 | 0x10_0000 => Flash::new(contents, [0; 20], false, logger)
            .map_err(|_| invalid())?
            .into(),
        _ => return Err(invalid()),
    })
}

// --- DeSmuME .dsv -----------------------------------------------------------------------------

const DSV_FOOTER_TEXT: &[u8] =
    b"|<--Snip above here to create a raw sav by excluding this DeSmuME savedata footer:";
const DSV_COOKIE: &[u8] = b"|-DESMUME SAVE-|";
/// size, padSize, type, addr_size, mem_size, version.
const DSV_INFO_LEN: usize = 6 * 4;

/// Strips a DeSmuME footer if present (what DeSmuME's `importData` does for `.dsv` files).
pub(crate) fn strip_dsv_footer(data: &[u8]) -> &[u8] {
    if !data.ends_with(DSV_COOKIE) {
        return data;
    }
    let Some(before_info) = data.len().checked_sub(DSV_COOKIE.len() + DSV_INFO_LEN) else {
        return data;
    };
    let data = &data[..before_info];
    data.strip_suffix(DSV_FOOTER_TEXT).unwrap_or(data)
}

fn dsv_footer(len: usize) -> Vec<u8> {
    // Index into DeSmuME's save_types table (0 = autodetect) and its address size.
    let (save_type, addr_size): (u32, u32) = match len {
        0x200 => (1, 1),
        0x2000 => (2, 2),
        0x1_0000 => (3, 2),
        0x8000 => (4, 2),
        0x4_0000 => (5, 3),
        0x8_0000 => (6, 3),
        0x10_0000 => (7, 3),
        _ => (0, 3),
    };
    let mut footer = DSV_FOOTER_TEXT.to_vec();
    for value in [len as u32, len as u32, save_type, addr_size, len as u32, 0] {
        footer.extend_from_slice(&value.to_le_bytes());
    }
    footer.extend_from_slice(DSV_COOKIE);
    footer
}

/// A raw save, padded with zeros to the next power of two (like the desktop frontend).
fn padded_save(raw: &[u8]) -> BoxedByteSlice {
    let mut save = BoxedByteSlice::new_zeroed(raw.len().next_power_of_two());
    save[..raw.len()].copy_from_slice(raw);
    save
}

/// Reads a `.sav` or `.dsv` file (a DeSmuME footer is stripped either way).
pub(crate) fn read_save_file(path: &Path) -> Result<BoxedByteSlice, Error> {
    let data = read_file(path)?;
    let raw = strip_dsv_footer(&data);
    if raw.is_empty() {
        return Err(Error::InvalidSave(format!("{} is empty", path.display())));
    }
    Ok(padded_save(raw))
}

impl Session {
    fn persistent_contents(&mut self) -> Result<Vec<u8>, Error> {
        let mut contents = Vec::new();
        PersistentWriteSavestate::new(&mut contents)
            .store(&mut self.emu)
            .map_err(|_| invalid_state("couldn't store the emulator state"))?;
        Ok(contents)
    }

    fn current_save(&self) -> Option<Vec<u8>> {
        let contents = self.emu.ds_slot.spi.contents();
        (!contents.is_empty()).then(|| contents.to_vec())
    }

    /// Puts `save` into the cartridge, replacing the SPI device when the size changes.
    fn put_save(&mut self, save: BoxedByteSlice) -> Result<(), Error> {
        if self.emu.ds_slot.spi.contents().len() == save.len() {
            self.emu
                .ds_slot
                .spi
                .reload_contents(SaveReloadContents::Existing(save));
        } else {
            self.emu.ds_slot.spi = spi_for_save(SaveContents::Existing(save), &self.logger)?;
        }
        Ok(())
    }

    fn load_decoded(&mut self, state: Decoded<'_>, save: Option<&[u8]>) -> Result<(), Error> {
        let save = save.or(state.save);
        // A failed load can leave the emulator half-loaded: keep what's needed to roll back.
        let backup = self.persistent_contents()?;
        let prev_spi = save.map(|_| self.emu.ds_slot.spi.clone());
        if let Some(save) = save {
            let mut contents = BoxedByteSlice::new_zeroed(save.len());
            contents.copy_from_slice(save);
            self.put_save(contents)?;
        }

        let loaded = PersistentReadSavestate::new(state.contents)
            .map_err(|_| ())
            .and_then(|mut savestate| savestate.load_into(&mut self.emu).map_err(drop));
        if loaded.is_err() {
            if let Some(prev_spi) = prev_spi {
                self.emu.ds_slot.spi = prev_spi;
            }
            PersistentReadSavestate::new(&backup)
                .and_then(|mut savestate| savestate.load_into(&mut self.emu).map_err(drop))
                .expect("couldn't restore the emulator state after a failed load");
            return Err(invalid_state(
                "couldn't load the state (from another ROM or dust version, or a save type \
                 mismatch?)",
            ));
        }

        if let Some(frame) = state.frame {
            self.frame = frame;
        }
        self.screen_override = Some(state.framebuffer);
        Ok(())
    }

    /// An in-memory state, for fast resets. `include_save` also copies the cartridge save.
    pub fn save_state_bytes(&mut self, include_save: bool) -> Result<StateBytes, Error> {
        let contents = self.persistent_contents()?;
        let state = encode(&contents, self.frame, None, self.framebuffer());
        Ok(StateBytes {
            state,
            save: include_save.then(|| self.current_save()).flatten(),
        })
    }

    /// Loads a state from [`Session::save_state_bytes`] (or an uncompressed state file); `save`
    /// replaces the cartridge save. On error the session is unchanged.
    pub fn load_state_bytes(&mut self, state: &[u8], save: Option<&[u8]>) -> Result<(), Error> {
        let state = decode(state)?;
        self.load_decoded(state, save)
    }

    /// Writes a state file the desktop GUI can load too.
    pub fn save_state(&mut self, path: impl AsRef<Path>, include_save: bool) -> Result<(), Error> {
        let contents = self.persistent_contents()?;
        let save = include_save.then(|| self.current_save()).flatten();
        let data = encode(&contents, self.frame, save.as_deref(), self.framebuffer());
        write_file(
            path.as_ref(),
            &compress_to_vec(&data, CompressionLevel::BestSpeed as u8),
        )
    }

    /// Loads a state file written by [`Session::save_state`] or the desktop GUI. On error the
    /// session is unchanged.
    pub fn load_state(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let data = decompress_to_vec(&read_file(path)?).map_err(|err| {
            invalid_state(format!("{} isn't a dust state file: {err}", path.display()))
        })?;
        let state = decode(&data)?;
        self.load_decoded(state, None)
    }

    /// The cartridge save; `None` if the game has none.
    pub fn save_data(&self) -> Option<Vec<u8>> {
        self.current_save()
    }

    /// Writes the cartridge save as a raw `.sav` or a DeSmuME `.dsv`.
    pub fn export_save(&self, path: impl AsRef<Path>, format: SaveFormat) -> Result<(), Error> {
        let mut data = self.current_save().ok_or(Error::NoSave)?;
        if format == SaveFormat::Dsv {
            let footer = dsv_footer(data.len());
            data.extend_from_slice(&footer);
        }
        write_file(path.as_ref(), &data)
    }

    /// Replaces the cartridge save with a `.sav` or `.dsv` file (a DeSmuME footer is stripped);
    /// the save type follows the file size. It is also kept across [`Session::reset`]. Like
    /// swapping saves on hardware, games usually only read it after a reset.
    pub fn import_save(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let save = read_save_file(path.as_ref())?;
        // Validate the size before touching the emulator.
        spi_for_save(SaveContents::New(save.len()), &self.logger)?;
        self.inputs.save = Some(save.clone());
        self.put_save(save)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsv_round_trip() {
        let raw = vec![0xA5; 0x2000];
        let mut dsv = raw.clone();
        dsv.extend_from_slice(&dsv_footer(raw.len()));
        assert_eq!(dsv.len(), raw.len() + 122);
        assert_eq!(strip_dsv_footer(&dsv), &raw[..]);
        assert_eq!(strip_dsv_footer(&raw), &raw[..]);
    }

    #[test]
    fn state_layout_round_trip() {
        let mut framebuffer = Box::new([[0; SCREEN_WIDTH * SCREEN_HEIGHT]; 2]);
        framebuffer[1][5] = 0xFF12_3456;
        let data = encode(b"contents", 42, Some(&[1, 2, 3, 4]), &framebuffer);
        let decoded = decode(&data).unwrap();
        assert_eq!(decoded.contents, b"contents");
        assert_eq!(decoded.frame, Some(42));
        assert_eq!(decoded.save, Some(&[1, 2, 3, 4][..]));
        assert_eq!(decoded.framebuffer[1][5], 0xFF12_3456);
    }

    /// The desktop GUI writes no frame counter and a native-endian framebuffer.
    #[test]
    fn decodes_gui_states() {
        let mut data = b"contents".to_vec();
        data.extend_from_slice(&[9; 0x200]);
        for i in 0..FRAMEBUFFER_LEN / 4 {
            data.extend_from_slice(&(i as u32).to_be_bytes());
        }
        data.extend_from_slice(&(INFO_HAS_SAVE | 0x200).to_le_bytes());
        let decoded = decode(&data).unwrap();
        assert_eq!(decoded.contents, b"contents");
        assert_eq!(decoded.frame, None);
        assert_eq!(decoded.save, Some(&[9; 0x200][..]));
        assert_eq!(decoded.framebuffer[0][3], 3);
        assert!(decode(&[0; 8]).is_err());
    }
}
