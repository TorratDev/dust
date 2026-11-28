//! Observability: named memory regions, disassembly, emulator health stats and captured core
//! logs.

use crate::{memory, Cpu, Error};
use dust_core::{
    cpu::{disasm, interpreter::Interpreter},
    emu::Emu,
    gpu::engine_3d::{Polygon, RendererTx, RenderingState, ScreenVertex},
    utils::mem_prelude::*,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    fmt::{self, Write as _},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

// --- Memory map -------------------------------------------------------------------------------

/// A named memory region from [`memory_map`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    pub name: &'static str,
    /// The CPU whose bus `start` refers to.
    pub cpu: Cpu,
    /// Where the region is (usually) mapped. Dumps of regions with their own storage (RAM, VRAM
    /// banks, palettes, OAM) read that storage whatever the current mapping; I/O regions are read
    /// through the CPU's bus.
    pub start: u32,
    pub size: u32,
}

#[derive(Clone, Copy)]
enum Source {
    MainRam,
    SharedWram,
    Arm7Wram,
    VramBank(u8),
    Palette,
    Oam,
    Bus,
}

struct RegionDef {
    name: &'static str,
    cpu: Cpu,
    start: u32,
    size: u32,
    source: Source,
}

const fn region(name: &'static str, cpu: Cpu, start: u32, size: u32, source: Source) -> RegionDef {
    RegionDef {
        name,
        cpu,
        start,
        size,
        source,
    }
}

/// Main RAM's size comes from the emulator; the 0 here is filled in by [`memory_map`].
const REGIONS: &[RegionDef] = &[
    region("main_ram", Cpu::Arm9, 0x0200_0000, 0, Source::MainRam),
    // The shared WRAM's mapping depends on WRAMCNT; dumps contain all of it.
    region(
        "shared_wram",
        Cpu::Arm9,
        0x0300_0000,
        0x8000,
        Source::SharedWram,
    ),
    region(
        "arm7_wram",
        Cpu::Arm7,
        0x0380_0000,
        0x1_0000,
        Source::Arm7Wram,
    ),
    // VRAM banks at their LCDC addresses.
    region(
        "vram_a",
        Cpu::Arm9,
        0x0680_0000,
        0x2_0000,
        Source::VramBank(0),
    ),
    region(
        "vram_b",
        Cpu::Arm9,
        0x0682_0000,
        0x2_0000,
        Source::VramBank(1),
    ),
    region(
        "vram_c",
        Cpu::Arm9,
        0x0684_0000,
        0x2_0000,
        Source::VramBank(2),
    ),
    region(
        "vram_d",
        Cpu::Arm9,
        0x0686_0000,
        0x2_0000,
        Source::VramBank(3),
    ),
    region(
        "vram_e",
        Cpu::Arm9,
        0x0688_0000,
        0x1_0000,
        Source::VramBank(4),
    ),
    region(
        "vram_f",
        Cpu::Arm9,
        0x0689_0000,
        0x4000,
        Source::VramBank(5),
    ),
    region(
        "vram_g",
        Cpu::Arm9,
        0x0689_4000,
        0x4000,
        Source::VramBank(6),
    ),
    region(
        "vram_h",
        Cpu::Arm9,
        0x0689_8000,
        0x8000,
        Source::VramBank(7),
    ),
    region(
        "vram_i",
        Cpu::Arm9,
        0x068A_0000,
        0x4000,
        Source::VramBank(8),
    ),
    // Engine A palettes at 0x0500_0000, engine B's at 0x0500_0400.
    region("palettes", Cpu::Arm9, 0x0500_0000, 0x800, Source::Palette),
    // Engine A OAM at 0x0700_0000, engine B's at 0x0700_0400.
    region("oam", Cpu::Arm9, 0x0700_0000, 0x800, Source::Oam),
    // Engine A, 3D and system registers up to 0x0400_06A3, engine B's at 0x0400_1000.
    region("arm9_io", Cpu::Arm9, 0x0400_0000, 0x1070, Source::Bus),
    // Includes the sound registers (0x0400_0400-0x0400_051F).
    region("arm7_io", Cpu::Arm7, 0x0400_0000, 0x520, Source::Bus),
];

fn region_size(emu: &Emu<Interpreter>, def: &RegionDef) -> u32 {
    match def.source {
        Source::MainRam => emu.main_mem_mask().get() + 1,
        _ => def.size,
    }
}

pub(crate) fn memory_map(emu: &Emu<Interpreter>) -> Vec<MemoryRegion> {
    REGIONS
        .iter()
        .map(|def| MemoryRegion {
            name: def.name,
            cpu: def.cpu,
            start: def.start,
            size: region_size(emu, def),
        })
        .collect()
}

fn copy(ptr: *const u8, len: u32) -> Vec<u8> {
    // Every caller passes a length within the storage's allocation.
    unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
}

pub(crate) fn dump(emu: &mut Emu<Interpreter>, name: &str) -> Result<Vec<u8>, Error> {
    let def = REGIONS.iter().find(|def| def.name == name).ok_or_else(|| {
        let names = REGIONS.iter().map(|def| def.name).collect::<Vec<_>>();
        Error::InvalidArgument(format!(
            "unknown memory region {name:?} (expected one of {})",
            names.join(", ")
        ))
    })?;
    let size = region_size(emu, def);
    let banks = &emu.gpu.vram.banks;
    Ok(match def.source {
        Source::MainRam => copy(emu.main_mem().as_ptr(), size),
        Source::SharedWram => copy(emu.swram.contents().as_ptr(), size),
        Source::Arm7Wram => copy(emu.arm7.wram.as_ptr(), size),
        Source::VramBank(bank) => copy(
            match bank {
                0 => banks.a.as_ptr(),
                1 => banks.b.as_ptr(),
                2 => banks.c.as_ptr(),
                3 => banks.d.as_ptr(),
                4 => banks.e.as_ptr(),
                5 => banks.f.as_ptr(),
                6 => banks.g.as_ptr(),
                7 => banks.h.as_ptr(),
                _ => banks.i.as_ptr(),
            },
            size,
        ),
        Source::Palette => copy(emu.gpu.vram.palette.as_ptr(), size),
        Source::Oam => copy(emu.gpu.vram.oam.as_ptr(), size),
        Source::Bus => memory::read(emu, def.cpu, def.start, size)?,
    })
}

// --- Disassembly ------------------------------------------------------------------------------

/// Most instructions one [`crate::Session::disasm`] call returns.
pub const MAX_DISASM_COUNT: u32 = 0x1_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    pub addr: u32,
    /// The instruction word (the low 16 bits in Thumb state).
    pub raw: u32,
    /// As the desktop GUI's disassembly view shows it: `opcode`, then ` ; comment` if any.
    pub text: String,
}

pub(crate) fn disasm(
    emu: &mut Emu<Interpreter>,
    cpu: Cpu,
    addr: u32,
    count: u32,
    thumb: bool,
) -> Result<Vec<Instruction>, Error> {
    if count > MAX_DISASM_COUNT {
        return Err(Error::InvalidArgument(format!(
            "{count} instructions is more than the {MAX_DISASM_COUNT} limit"
        )));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let size_shift = 2 - thumb as u32;
    let start = addr & !((1 << size_shift) - 1);
    let end = start.wrapping_add((count - 1) << size_shift);
    let mut instrs = Vec::with_capacity(count as usize);
    match cpu {
        Cpu::Arm9 => disasm::disassemble_range::<_, true>(emu, (start, end), thumb, &mut instrs),
        Cpu::Arm7 => disasm::disassemble_range::<_, false>(emu, (start, end), thumb, &mut instrs),
    }
    Ok(instrs
        .into_iter()
        .map(|instr| Instruction {
            addr: instr.addr,
            raw: instr.raw,
            text: if instr.comment.is_empty() {
                instr.opcode
            } else {
                format!("{} ; {}", instr.opcode, instr.comment)
            },
        })
        .collect())
}

// --- Stats ------------------------------------------------------------------------------------

/// The DS's refresh rate: 33.513982 MHz / (6 * 355 * 263) cycles per frame.
pub const DS_FPS: f64 = 33_513_982.0 / (6.0 * 355.0 * 263.0);
/// Frames [`crate::Session::stats`] averages over by default.
pub const DEFAULT_STATS_WINDOW: u32 = 60;
/// Frame times kept; larger windows are clamped to this.
pub const MAX_STATS_WINDOW: u32 = 3600;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    /// Frames emulated per second of emulation time over the window (time spent outside of
    /// emulation, such as between `step` calls, doesn't count).
    pub fps: f64,
    /// `fps` relative to the DS's ~59.83 FPS.
    pub speed_percent: f64,
    pub frame_time_avg_us: f64,
    pub frame_time_max_us: f64,
    /// Frames that took longer to emulate than they last on a DS, since the session was created.
    pub late_frames: u64,
    /// 3D frames the game didn't finish in time (no SWAP_BUFFERS before VBlank, so the previous
    /// 3D frame was shown again), since the session was created.
    pub dropped_frames: u64,
    /// Frames emulated since the session was created (across resets and state loads).
    pub total_frames: u64,
    /// Polygons and vertices in the last 3D frame the game swapped in.
    pub polygons: u32,
    pub vertices: u32,
    /// The hardware's shared polygon/vertex RAM overflow flag (DISP3DCNT bit 13, sticky until
    /// the game acknowledges it) as of the last swapped 3D frame, attributed to polygon RAM when
    /// it was full (2048 polygons) and to vertex RAM otherwise.
    pub polygon_ram_overflow: bool,
    pub vertex_ram_overflow: bool,
    /// Commands in the geometry FIFO right now (0-256, as GXSTAT reports it).
    pub geometry_fifo_level: u32,
}

#[derive(Clone, Copy, Default)]
struct Frame3d {
    polygons: u32,
    vertices: u32,
    overflow: bool,
}

#[derive(Default)]
pub(crate) struct Shared3d {
    last: Cell<Frame3d>,
    repeated: Cell<u64>,
}

/// Wraps the 3D renderer to record what the game sends it.
pub(crate) struct StatsTx {
    inner: Box<dyn RendererTx>,
    shared: Rc<Shared3d>,
}

impl StatsTx {
    pub fn new(inner: Box<dyn RendererTx>, shared: Rc<Shared3d>) -> Self {
        StatsTx { inner, shared }
    }
}

impl RendererTx for StatsTx {
    fn set_capture_enabled(&mut self, capture_enabled: bool) {
        self.inner.set_capture_enabled(capture_enabled);
    }

    fn swap_buffers(
        &mut self,
        vert_ram: &[ScreenVertex],
        poly_ram: &[Polygon],
        state: &RenderingState,
    ) {
        self.shared.last.set(Frame3d {
            polygons: poly_ram.len() as u32,
            vertices: vert_ram.len() as u32,
            overflow: state.control.poly_vert_ram_overflow(),
        });
        self.inner.swap_buffers(vert_ram, poly_ram, state);
    }

    fn repeat_last_frame(&mut self, state: &RenderingState) {
        self.shared.repeated.set(self.shared.repeated.get() + 1);
        self.inner.repeat_last_frame(state);
    }

    fn start_rendering(
        &mut self,
        texture: &Bytes<0x8_0000>,
        tex_pal: &Bytes<0x1_8000>,
        state: &RenderingState,
    ) {
        self.inner.start_rendering(texture, tex_pal, state);
    }

    fn skip_rendering(&mut self) {
        self.inner.skip_rendering();
    }
}

pub(crate) struct StatsTracker {
    frame_times: VecDeque<Duration>,
    late_frames: u64,
    total_frames: u64,
    pub shared_3d: Rc<Shared3d>,
}

impl Default for StatsTracker {
    fn default() -> Self {
        StatsTracker {
            frame_times: VecDeque::with_capacity(MAX_STATS_WINDOW as usize),
            late_frames: 0,
            total_frames: 0,
            shared_3d: Rc::default(),
        }
    }
}

impl StatsTracker {
    pub fn record_frame(&mut self, time: Duration) {
        if self.frame_times.len() == MAX_STATS_WINDOW as usize {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(time);
        self.total_frames += 1;
        if time.as_secs_f64() > 1.0 / DS_FPS {
            self.late_frames += 1;
        }
    }

    pub fn stats(&self, emu: &Emu<Interpreter>, window: u32) -> Stats {
        let window = match window {
            0 => DEFAULT_STATS_WINDOW,
            window => window.min(MAX_STATS_WINDOW),
        } as usize;
        let times = self
            .frame_times
            .iter()
            .skip(self.frame_times.len().saturating_sub(window));
        let (count, total, max) = times.fold((0, Duration::ZERO, Duration::ZERO), |acc, &time| {
            (acc.0 + 1, acc.1 + time, acc.2.max(time))
        });
        let avg_us = if count == 0 {
            0.0
        } else {
            total.as_secs_f64() * 1e6 / count as f64
        };
        let fps = if avg_us == 0.0 { 0.0 } else { 1e6 / avg_us };
        let frame_3d = self.shared_3d.last.get();
        let polygon_ram_overflow = frame_3d.overflow && frame_3d.polygons >= 2048;
        Stats {
            fps,
            speed_percent: fps / DS_FPS * 100.0,
            frame_time_avg_us: avg_us,
            frame_time_max_us: max.as_secs_f64() * 1e6,
            late_frames: self.late_frames,
            dropped_frames: self.shared_3d.repeated.get(),
            total_frames: self.total_frames,
            polygons: frame_3d.polygons,
            vertices: frame_3d.vertices,
            polygon_ram_overflow,
            vertex_ram_overflow: frame_3d.overflow && !polygon_ram_overflow,
            geometry_fifo_level: emu.gpu.engine_3d.gx_status().fifo_level() as u32,
        }
    }
}

// --- Logs -------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    #[default]
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

impl From<slog::Level> for LogLevel {
    fn from(level: slog::Level) -> Self {
        match level {
            slog::Level::Trace => LogLevel::Trace,
            slog::Level::Debug => LogLevel::Debug,
            slog::Level::Info => LogLevel::Info,
            slog::Level::Warning => LogLevel::Warning,
            slog::Level::Error | slog::Level::Critical => LogLevel::Error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// Increases by one per entry, from 1, for the session's whole life.
    pub seq: u64,
    /// Frames finished when the entry was logged (the session's frame counter).
    pub frame: u64,
    pub level: LogLevel,
    pub message: String,
    /// The logging component's context, e.g. `cpu = "arm9"`, `gpu`, `spi`, `ds_rom = "normal"`
    /// (keys without a value map to an empty string), plus any per-message key-values.
    pub fields: BTreeMap<String, String>,
}

/// Entries kept; older ones are dropped.
pub const LOG_CAPACITY: usize = 4096;

/// Called with every new entry; returning `false` unsubscribes.
pub type LogSubscriber = Box<dyn FnMut(&LogEntry) -> bool + Send>;

#[derive(Default)]
struct LogState {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
    subscribers: Vec<LogSubscriber>,
}

#[derive(Clone, Default)]
pub(crate) struct LogCapture {
    state: Arc<Mutex<LogState>>,
    /// The session's frame counter, mirrored for the drain.
    pub frame: Arc<AtomicU64>,
}

impl LogCapture {
    pub fn logger(&self) -> slog::Logger {
        slog::Logger::root(
            LogDrain {
                capture: self.clone(),
            },
            slog::o!(),
        )
    }

    pub fn entries(&self, since: u64, min_level: LogLevel) -> Vec<LogEntry> {
        self.state
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter(|entry| entry.seq > since && entry.level >= min_level)
            .cloned()
            .collect()
    }

    pub fn subscribe(&self, subscriber: LogSubscriber) {
        self.state.lock().unwrap().subscribers.push(subscriber);
    }

    fn push(&self, level: LogLevel, message: String, fields: BTreeMap<String, String>) {
        let mut state = self.state.lock().unwrap();
        state.next_seq += 1;
        let entry = LogEntry {
            seq: state.next_seq,
            frame: self.frame.load(Ordering::Relaxed),
            level,
            message,
            fields,
        };
        state
            .subscribers
            .retain_mut(|subscriber| subscriber(&entry));
        if state.entries.len() == LOG_CAPACITY {
            state.entries.pop_front();
        }
        state.entries.push_back(entry);
    }
}

struct LogDrain {
    capture: LogCapture,
}

struct FieldCollector(BTreeMap<String, String>);

impl slog::Serializer for FieldCollector {
    fn emit_arguments(&mut self, key: slog::Key, value: &fmt::Arguments) -> slog::Result {
        let mut text = String::new();
        let _ = text.write_fmt(*value);
        self.0.insert(key.to_string(), text);
        Ok(())
    }
}

impl slog::Drain for LogDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record,
        values: &slog::OwnedKVList,
    ) -> Result<Self::Ok, Self::Err> {
        use slog::KV;
        let mut fields = FieldCollector(BTreeMap::new());
        let _ = values.serialize(record, &mut fields);
        let _ = record.kv().serialize(record, &mut fields);
        self.capture
            .push(record.level().into(), record.msg().to_string(), fields.0);
        Ok(())
    }
}
