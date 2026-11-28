use crate::{
    breakpoints::Hooks,
    memory::{self, Cpu, Regs, WatchChange, Watches},
    rtc, soft_renderer_3d, Keys,
};
use chrono::{DateTime, NaiveDateTime};
use dust_core::{
    audio::DummyBackend as DummyAudioBackend,
    cpu::{arm7, arm9, interpreter::Interpreter},
    ds_slot::{self, rom::header::Header},
    emu,
    flash::Flash,
    gpu::{engine_2d, engine_3d},
    spi::firmware,
    utils::{zeroed_box, BoxedByteSlice, Bytes},
    Model, SaveContents,
};
use std::{fmt, fs, io, path::PathBuf};

/// Options for [`Session::new`].
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// Firmware dump. Without one, dust's built-in default firmware is used. The boot seed of many
    /// games comes from the firmware MAC, so pass the same dump as other emulators to match them.
    pub firmware: Option<PathBuf>,
    /// ARM7 BIOS dump. Booting through the firmware needs both BIOS files; without them the ROM is
    /// booted directly.
    pub arm7_bios: Option<PathBuf>,
    /// ARM9 BIOS dump, see `arm7_bios`.
    pub arm9_bios: Option<PathBuf>,
    /// Fixed RTC time (Unix seconds, read as UTC wall-clock time). The clock does not advance, so
    /// runs are reproducible. `None` follows the host's local clock.
    pub rtc_unix_time: Option<i64>,
}

/// Who controls the emulator's input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Control {
    #[default]
    Agent,
    Human,
    Shared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Frames emulated since the session was created or last reset.
    pub frame: u64,
    pub game_title: String,
    pub paused: bool,
    pub control: Control,
}

/// Result of [`Session::step`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    pub status: Status,
    /// Watched ranges that changed since the previous step (writes made in between included).
    pub changes: Vec<WatchChange>,
}

#[derive(Debug)]
pub enum Error {
    Io {
        path: PathBuf,
        err: io::Error,
    },
    InvalidFirmware(String),
    InvalidBios(String),
    InvalidRtcTime(i64),
    InvalidRom(String),
    /// A touch position outside the 256x192 bottom screen.
    InvalidTouch {
        x: u16,
        y: u16,
    },
    /// The emulated system powered off; reset the session to continue.
    Shutdown,
    /// A bad memory/register request (size, register index, watch id).
    InvalidArgument(String),
    /// No breakpoint or watchpoint with this id.
    NoSuchHook(u32),
    InvalidHook(String),
    /// Emulation can't be advanced or reset from a hook hit's handler; continue first.
    StoppedAtHook,
    /// Continue was requested while not stopped at a hook hit.
    NotStopped,
    /// The hit handler aborted the step; the emulator is left mid-frame.
    Interrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, err } => write!(f, "couldn't read {}: {err}", path.display()),
            Error::InvalidFirmware(msg) => write!(f, "invalid firmware: {msg}"),
            Error::InvalidBios(msg) => write!(f, "invalid BIOS: {msg}"),
            Error::InvalidRtcTime(time) => write!(f, "invalid RTC time: {time}"),
            Error::InvalidRom(msg) => write!(f, "invalid ROM: {msg}"),
            Error::InvalidTouch { x, y } => {
                write!(f, "touch position ({x}, {y}) is outside the 256x192 screen")
            }
            Error::Shutdown => f.write_str("the emulated system shut down"),
            Error::InvalidArgument(msg) => f.write_str(msg),
            Error::NoSuchHook(id) => write!(f, "no breakpoint or watchpoint with id {id}"),
            Error::InvalidHook(msg) => write!(f, "invalid hook: {msg}"),
            Error::StoppedAtHook => {
                f.write_str("stopped at a breakpoint or watchpoint; continue first")
            }
            Error::NotStopped => f.write_str("not stopped at a breakpoint or watchpoint"),
            Error::Interrupted => f.write_str("the step was interrupted at a hook hit"),
        }
    }
}

impl std::error::Error for Error {}

fn read(path: &PathBuf) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|err| Error::Io {
        path: path.clone(),
        err,
    })
}

fn read_bios<const LEN: usize>(path: &Option<PathBuf>) -> Result<Option<Vec<u8>>, Error> {
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes = read(path)?;
    if bytes.len() != LEN {
        return Err(Error::InvalidBios(format!(
            "{} is {} B, expected {LEN} B",
            path.display(),
            bytes.len()
        )));
    }
    Ok(Some(bytes))
}

fn boxed_bios<const LEN: usize>(bytes: &Option<Vec<u8>>) -> Option<Box<Bytes<LEN>>> {
    bytes.as_ref().map(|bytes| {
        let mut bios = zeroed_box::<Bytes<LEN>>();
        bios.copy_from_slice(bytes);
        bios
    })
}

fn boxed_slice(bytes: &[u8], len: usize) -> BoxedByteSlice {
    let mut slice = BoxedByteSlice::new_zeroed(len);
    slice[..bytes.len()].copy_from_slice(bytes);
    slice
}

/// Everything needed to (re)build the emulator, loaded once from disk.
struct Inputs {
    rom: Vec<u8>,
    firmware: Option<Vec<u8>>,
    arm7_bios: Option<Vec<u8>>,
    arm9_bios: Option<Vec<u8>>,
    model: Model,
    rtc_base: Option<NaiveDateTime>,
}

impl Inputs {
    fn load(rom: &PathBuf, opts: &SessionOptions) -> Result<Self, Error> {
        let rom = read(rom)?;
        if rom.len() < 0x200 {
            return Err(Error::InvalidRom(format!(
                "{} B is too small for a DS ROM",
                rom.len()
            )));
        }

        let firmware = opts.firmware.as_ref().map(read).transpose()?;
        let model = match &firmware {
            Some(firmware_bytes) => {
                if !firmware::is_valid_size(firmware_bytes.len()) {
                    return Err(Error::InvalidFirmware(format!(
                        "unexpected size: {} B",
                        firmware_bytes.len()
                    )));
                }
                firmware::detect_model(firmware_bytes)
                    .map_err(|err| Error::InvalidFirmware(format!("{err:?}")))?
                    .unwrap_or_default()
            }
            None => Model::default(),
        };

        let rtc_base = opts
            .rtc_unix_time
            .map(|time| {
                DateTime::from_timestamp(time, 0)
                    .map(|date_time| date_time.naive_utc())
                    .ok_or(Error::InvalidRtcTime(time))
            })
            .transpose()?;

        Ok(Inputs {
            rom,
            firmware,
            arm7_bios: read_bios::<{ arm7::BIOS_SIZE }>(&opts.arm7_bios)?,
            arm9_bios: read_bios::<{ arm9::BIOS_SIZE }>(&opts.arm9_bios)?,
            model,
            rtc_base,
        })
    }

    fn game_title(&self) -> String {
        let header: &[u8; 0x170] = self.rom[..0x170].try_into().unwrap();
        Header::new(header)
            .game_title()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn build_emu(&self) -> Result<emu::Emu<Interpreter>, Error> {
        let firmware = match &self.firmware {
            Some(firmware) => boxed_slice(firmware, firmware.len()),
            None => firmware::default(self.model),
        };
        let firmware = Flash::new(
            SaveContents::Existing(firmware),
            firmware::id_for_model(self.model),
        )
        .map_err(|err| Error::InvalidFirmware(format!("{err:?}")))?;

        let rom: Box<dyn ds_slot::rom::Contents> =
            Box::new(boxed_slice(&self.rom, self.rom.len().next_power_of_two()));

        let (renderer_3d_tx, renderer_3d_rx) = soft_renderer_3d::init();
        let renderer_2d = dust_soft_2d::sync::Renderer::new(Box::new(renderer_3d_rx));

        let mut builder = emu::Builder::new(
            firmware,
            Some(rom),
            ds_slot::spi::Empty::new().into(),
            Box::new(DummyAudioBackend),
            None,
            Box::new(rtc::Backend::new(self.rtc_base)),
            Box::new(renderer_2d) as Box<dyn engine_2d::Renderer>,
            Box::new(renderer_3d_tx) as Box<dyn engine_3d::RendererTx>,
            None,
        );
        builder.arm7_bios = boxed_bios(&self.arm7_bios);
        builder.arm9_bios = boxed_bios(&self.arm9_bios);
        builder.model = self.model;
        // Booting through the firmware needs both BIOS files.
        builder.direct_boot = self.arm7_bios.is_none() || self.arm9_bios.is_none();

        builder.build(Interpreter).map_err(|err| match err {
            emu::BuildError::MissingRom => Error::InvalidRom("missing ROM".to_owned()),
            emu::BuildError::MissingSysFiles => {
                Error::InvalidBios("both BIOS files are needed to boot the firmware".to_owned())
            }
            emu::BuildError::RomCreation(ds_slot::rom::normal::CreationError::InvalidSize) => {
                Error::InvalidRom("invalid size".to_owned())
            }
            emu::BuildError::RomNeedsDecryptionButNoBiosProvided => {
                Error::InvalidRom("the ROM needs decryption, which needs the BIOS".to_owned())
            }
        })
    }
}

/// One emulator instance, advanced only by [`Session::step`].
pub struct Session {
    inputs: Inputs,
    pub(crate) emu: emu::Emu<Interpreter>,
    game_title: String,
    pub(crate) frame: u64,
    paused: bool,
    control: Control,
    watches: Watches,
    /// Agent input, applied before every frame (see `input.rs`).
    pub(crate) keys: Keys,
    pub(crate) touch: Option<[u16; 2]>,
    /// Breakpoints and watchpoints (see `breakpoints.rs`).
    pub(crate) hooks: Hooks,
}

impl Session {
    pub fn new(rom: impl Into<PathBuf>, opts: &SessionOptions) -> Result<Self, Error> {
        let inputs = Inputs::load(&rom.into(), opts)?;
        let mut emu = inputs.build_emu()?;
        let mut hooks = Hooks::default();
        hooks.install(&mut emu);
        Ok(Session {
            game_title: inputs.game_title(),
            inputs,
            emu,
            frame: 0,
            paused: false,
            control: Control::default(),
            watches: Watches::default(),
            keys: Keys::empty(),
            touch: None,
            hooks,
        })
    }

    /// Emulates `frames` whole frames. Stepping works while paused: pausing only stops
    /// free-running hosts (such as the desktop GUI) from advancing on their own. Breakpoint and
    /// watchpoint hits call the hit handler mid-frame (see [`Session::set_hit_handler`]).
    pub fn step(&mut self, frames: u32) -> Result<StepResult, Error> {
        for _ in 0..frames {
            self.apply_input();
            self.run_frame()?;
            self.frame += 1;
        }
        Ok(StepResult {
            status: self.status(),
            changes: self.watches.changes(&mut self.emu),
        })
    }

    pub fn pause(&mut self) {
        self.paused = true;
    }

    pub fn resume(&mut self) {
        self.paused = false;
    }

    /// Rebuilds the emulator from the same ROM and options; the frame counter restarts at 0 and
    /// agent input is released.
    pub fn reset(&mut self) -> Result<(), Error> {
        if self.current_hit().is_some() {
            return Err(Error::StoppedAtHook);
        }
        self.emu = self.inputs.build_emu()?;
        self.hooks.install(&mut self.emu);
        self.frame = 0;
        self.keys = Keys::empty();
        self.touch = None;
        Ok(())
    }

    pub fn status(&self) -> Status {
        Status {
            frame: self.frame,
            game_title: self.game_title.clone(),
            paused: self.paused,
            control: self.control,
        }
    }

    pub fn read(&mut self, cpu: Cpu, addr: u32, len: u32) -> Result<Vec<u8>, Error> {
        memory::read(&mut self.emu, cpu, addr, len)
    }

    /// Writes `data` byte by byte.
    pub fn write(&mut self, cpu: Cpu, addr: u32, data: &[u8]) -> Result<(), Error> {
        memory::write(&mut self.emu, cpu, addr, data)
    }

    /// Writes `value` as a single `size`-byte (1, 2 or 4) access.
    pub fn write_value(&mut self, cpu: Cpu, addr: u32, value: u32, size: u32) -> Result<(), Error> {
        memory::write_value(&mut self.emu, cpu, addr, value, size)
    }

    pub fn regs(&self, cpu: Cpu) -> Regs {
        memory::regs(&self.emu, cpu)
    }

    /// `reg` 0-14 are r0-r14; 15 ([`memory::REG_PC`]) jumps to `value` as the next instruction;
    /// 16 ([`memory::REG_CPSR`]) is cpsr.
    pub fn set_reg(&mut self, cpu: Cpu, reg: u32, value: u32) -> Result<(), Error> {
        memory::set_reg(&mut self.emu, cpu, reg, value)
    }

    /// Watches `len` bytes at `addr`; changes are reported by [`Session::step`].
    pub fn watch(&mut self, cpu: Cpu, addr: u32, len: u32) -> Result<u32, Error> {
        self.watches.add(&mut self.emu, cpu, addr, len)
    }

    pub fn unwatch(&mut self, watch_id: u32) -> Result<(), Error> {
        self.watches.remove(watch_id)
    }

    pub fn emu(&mut self) -> &mut emu::Emu<Interpreter> {
        &mut self.emu
    }
}
