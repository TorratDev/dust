//! Breakpoints (exec hooks) and write watchpoints.
//!
//! Hooks are registered on the [`Session`] and survive [`Session::reset`]. When one fires, the
//! emulator stops mid-frame right after the instruction that triggered it (before the instruction
//! at a breakpoint's address runs; after the store that hit a watchpoint) and the session's hit
//! handler (see [`Session::set_hit_handler`]) is called once per hit, with the CPU registers and
//! the results of the reads registered with the hook. Emulation goes on when the handler returns.
//!
//! The emulator side reuses dust-core's debugger hooks, as set up by the desktop GDB server
//! (`frontend/desktop/src/emu/gdb_server.rs`).

use crate::{
    memory::{self, Cpu, Regs},
    Error, Session,
};
use dust_core::{
    cpu::{
        debug::{
            BreakpointHook, MemWatchpointHook, MemWatchpointRwMask, MemWatchpointTriggerCause,
        },
        interpreter::Interpreter,
    },
    emu::{self, RunOutput},
    utils::schedule::RawTimestamp,
};
use std::{
    cell::RefCell,
    ops::ControlFlow,
    rc::Rc,
    time::{Duration, Instant},
};

/// Largest write watchpoint, in bytes.
pub const MAX_WATCH_SIZE: u32 = 0x1_0000;
/// Largest total length of the reads registered with one hook, in bytes.
pub const MAX_HOOK_READ_LEN: u32 = 0x10_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemRange {
    pub addr: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    Breakpoint,
    WriteWatchpoint,
}

/// A registered breakpoint or write watchpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub id: u32,
    pub kind: HookKind,
    pub cpu: Cpu,
    pub addr: u32,
    /// Watched bytes for watchpoints; 0 for breakpoints.
    pub size: u32,
    /// Read (in the hook's CPU address space) on every hit and sent with it.
    pub reads: Vec<MemRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookHit {
    pub id: u32,
    pub kind: HookKind,
    pub cpu: Cpu,
    /// Executed address for breakpoints, written address for watchpoints.
    pub addr: u32,
    /// Written value for watchpoints (little-endian, as wide as the store); 0 for breakpoints.
    pub value: u32,
    /// Frames finished before the hit.
    pub frame: u64,
    /// As [`Session::regs`] reports them: r15 reads ahead of the next instruction to execute.
    pub regs: Regs,
    /// Results of the hook's reads, in order.
    pub reads: Vec<Vec<u8>>,
}

/// Called on every hook hit; returning `ControlFlow::Break` aborts the running step with
/// [`Error::Interrupted`].
pub type HitHandler = Box<dyn FnMut(&mut Session, &HookHit) -> ControlFlow<()>>;

#[derive(Clone, Copy, Debug)]
enum Trigger {
    Exec { addr: u32 },
    Write { addr: u32, size: u8 },
}

/// State shared with the closures installed in the emulator.
#[derive(Default)]
struct Shared {
    /// Triggers since the last stop, per core.
    triggers: [Vec<Trigger>; 2],
}

#[derive(Default)]
pub(crate) struct Hooks {
    hooks: Vec<Hook>,
    next_id: u32,
    shared: Rc<RefCell<Shared>>,
    handler: Option<HitHandler>,
    /// The hit being handled, while the handler runs.
    current: Option<HookHit>,
    continue_requested: bool,
}

macro_rules! core_hooks {
    ($emu: expr, $shared: expr, $core: ident, $cpu: expr) => {{
        let shared = Rc::clone($shared);
        $emu.$core
            .set_breakpoint_hook(Some(BreakpointHook::new(Box::new(move |_emu, addr| {
                shared.borrow_mut().triggers[$cpu as usize].push(Trigger::Exec { addr });
                true
            }))));
        let shared = Rc::clone($shared);
        $emu.$core
            .set_mem_watchpoint_hook(Some(MemWatchpointHook::new(Box::new(
                move |_emu, addr, size, cause| {
                    if cause != MemWatchpointTriggerCause::Write {
                        return false;
                    }
                    shared.borrow_mut().triggers[$cpu as usize].push(Trigger::Write { addr, size });
                    true
                },
            ))));
    }};
}

fn read_bytes(emu: &mut emu::Emu<Interpreter>, cpu: Cpu, range: MemRange) -> Vec<u8> {
    memory::read_unchecked(emu, cpu, range.addr, range.len)
}

/// Address of the instruction `cpu` executes next.
fn next_pc(emu: &emu::Emu<Interpreter>, cpu: Cpu) -> u32 {
    let regs = memory::regs(emu, cpu);
    let thumb = regs.cpsr & 1 << 5 != 0;
    regs.gprs[15].wrapping_sub(8 >> thumb as u8)
}

fn add_breakpoint(emu: &mut emu::Emu<Interpreter>, cpu: Cpu, addr: u32) {
    match cpu {
        Cpu::Arm9 => emu.arm9.add_breakpoint(addr),
        Cpu::Arm7 => emu.arm7.add_breakpoint(addr),
    }
}

fn watch_bytes(emu: &mut emu::Emu<Interpreter>, cpu: Cpu, addr: u32, size: u32, add: bool) {
    for i in 0..size {
        let addr = addr.wrapping_add(i);
        match (cpu, add) {
            (Cpu::Arm9, true) => emu
                .arm9
                .add_mem_watchpoint(addr, 1, MemWatchpointRwMask::WRITE),
            (Cpu::Arm9, false) => {
                emu.arm9
                    .remove_mem_watchpoint(addr, 1, MemWatchpointRwMask::WRITE)
            }
            (Cpu::Arm7, true) => emu
                .arm7
                .add_mem_watchpoint(addr, 1, MemWatchpointRwMask::WRITE),
            (Cpu::Arm7, false) => {
                emu.arm7
                    .remove_mem_watchpoint(addr, 1, MemWatchpointRwMask::WRITE)
            }
        }
    }
}

fn overlaps(start: u32, len: u32, other_start: u32, other_len: u32) -> bool {
    let (start, other_start) = (start as u64, other_start as u64);
    start < other_start + other_len as u64 && other_start < start + len as u64
}

fn check_reads(reads: &[MemRange]) -> Result<(), Error> {
    let total: u64 = reads.iter().map(|range| range.len as u64).sum();
    if total > MAX_HOOK_READ_LEN as u64 {
        return Err(Error::InvalidHook(format!(
            "reads total {total} B, more than {MAX_HOOK_READ_LEN} B"
        )));
    }
    Ok(())
}

impl Hooks {
    /// Installs the hook closures and every registered hook in a freshly built emulator.
    pub(crate) fn install(&mut self, emu: &mut emu::Emu<Interpreter>) {
        *self.shared.borrow_mut() = Shared::default();
        core_hooks!(emu, &self.shared, arm9, Cpu::Arm9);
        core_hooks!(emu, &self.shared, arm7, Cpu::Arm7);
        for hook in &self.hooks {
            match hook.kind {
                HookKind::Breakpoint => add_breakpoint(emu, hook.cpu, hook.addr),
                HookKind::WriteWatchpoint => watch_bytes(emu, hook.cpu, hook.addr, hook.size, true),
            }
        }
    }

    fn add(&mut self, hook: Hook) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.hooks.push(Hook { id, ..hook });
        id
    }
}

impl Session {
    fn check_not_stopped(&self) -> Result<(), Error> {
        if self.hooks.current.is_some() {
            return Err(Error::StoppedAtHook);
        }
        Ok(())
    }

    /// Stops before `cpu` executes the instruction at `addr` (bit 0 is ignored, so Thumb addresses
    /// may have it set). Returns the hook id.
    pub fn break_add(&mut self, cpu: Cpu, addr: u32, reads: Vec<MemRange>) -> Result<u32, Error> {
        check_reads(&reads)?;
        let addr = addr & !1;
        add_breakpoint(&mut self.emu, cpu, addr);
        Ok(self.hooks.add(Hook {
            id: 0,
            kind: HookKind::Breakpoint,
            cpu,
            addr,
            size: 0,
            reads,
        }))
    }

    /// Stops after `cpu` (or a DMA transfer of its DMA controller) stores to any of the `size`
    /// bytes from `addr`. Returns the hook id.
    pub fn watch_write_add(
        &mut self,
        cpu: Cpu,
        addr: u32,
        size: u32,
        reads: Vec<MemRange>,
    ) -> Result<u32, Error> {
        if size == 0 || size > MAX_WATCH_SIZE {
            return Err(Error::InvalidHook(format!(
                "watchpoint size must be 1 to {MAX_WATCH_SIZE} B, got {size}"
            )));
        }
        if addr.checked_add(size - 1).is_none() {
            return Err(Error::InvalidHook(
                "watchpoint range wraps around the address space".to_owned(),
            ));
        }
        check_reads(&reads)?;
        watch_bytes(&mut self.emu, cpu, addr, size, true);
        Ok(self.hooks.add(Hook {
            id: 0,
            kind: HookKind::WriteWatchpoint,
            cpu,
            addr,
            size,
            reads,
        }))
    }

    fn hook_remove(&mut self, id: u32, kind: HookKind) -> Result<(), Error> {
        let i = self
            .hooks
            .hooks
            .iter()
            .position(|hook| hook.id == id && hook.kind == kind)
            .ok_or(Error::NoSuchHook(id))?;
        let hook = self.hooks.hooks.remove(i);
        let others = || {
            self.hooks
                .hooks
                .iter()
                .filter(|other| other.kind == kind && other.cpu == hook.cpu)
        };
        match kind {
            HookKind::Breakpoint => {
                if !others().any(|other| other.addr == hook.addr) {
                    match hook.cpu {
                        Cpu::Arm9 => self.emu.arm9.remove_breakpoint(hook.addr),
                        Cpu::Arm7 => self.emu.arm7.remove_breakpoint(hook.addr),
                    }
                }
            }
            HookKind::WriteWatchpoint => {
                // The emulator tracks watched bytes, not watchpoints: unwatch the range, then
                // watch again what other watchpoints still cover.
                let overlapping: Vec<_> = others()
                    .filter(|other| overlaps(hook.addr, hook.size, other.addr, other.size))
                    .map(|other| (other.addr, other.size))
                    .collect();
                watch_bytes(&mut self.emu, hook.cpu, hook.addr, hook.size, false);
                for (addr, size) in overlapping {
                    watch_bytes(&mut self.emu, hook.cpu, addr, size, true);
                }
            }
        }
        Ok(())
    }

    pub fn break_remove(&mut self, id: u32) -> Result<(), Error> {
        self.hook_remove(id, HookKind::Breakpoint)
    }

    pub fn watch_write_remove(&mut self, id: u32) -> Result<(), Error> {
        self.hook_remove(id, HookKind::WriteWatchpoint)
    }

    /// Registered breakpoints and watchpoints, oldest first.
    pub fn hooks(&self) -> &[Hook] {
        &self.hooks.hooks
    }

    /// Sets the function called on every hook hit. Without one, hits are ignored.
    pub fn set_hit_handler(&mut self, handler: Option<HitHandler>) {
        self.hooks.handler = handler;
    }

    /// The hit being handled, while the hit handler runs.
    pub fn current_hit(&self) -> Option<&HookHit> {
        self.hooks.current.as_ref()
    }

    /// Asks a waiting hit handler to let emulation go on (see [`Session::take_continue`]); fails
    /// unless the session is stopped at a hit.
    pub fn request_continue(&mut self) -> Result<(), Error> {
        if self.hooks.current.is_none() {
            return Err(Error::NotStopped);
        }
        self.hooks.continue_requested = true;
        Ok(())
    }

    /// Whether [`Session::request_continue`] was called since the current hit started, clearing
    /// the request.
    pub fn take_continue(&mut self) -> bool {
        std::mem::take(&mut self.hooks.continue_requested)
    }

    /// Emulates until the current frame finishes, handling hook hits on the way.
    pub(crate) fn run_frame(&mut self) -> Result<(), Error> {
        self.check_not_stopped()?;
        // Emulation time only: time stopped at hooks doesn't count towards the frame's stats.
        let mut emulation_time = Duration::ZERO;
        loop {
            // Effectively unlimited cycles: unlike `Emu::run`, this resumes mid-frame correctly
            // after a stop.
            let mut cycles: [RawTimestamp; 2] = [1 << 60; 2];
            let start = Instant::now();
            let output = self.emu.run_with_cycles(&mut cycles);
            emulation_time += start.elapsed();
            match output {
                RunOutput::FrameFinished => {
                    self.stats.record_frame(emulation_time);
                    return Ok(());
                }
                RunOutput::Shutdown => return Err(Error::Shutdown),
                RunOutput::StoppedByDebugHook => self.handle_stop()?,
                RunOutput::CyclesOver(_) => {}
            }
        }
    }

    fn hits_for(&mut self, cpu: Cpu, trigger: Trigger) -> Vec<HookHit> {
        let regs = memory::regs(&self.emu, cpu);
        let (kind, addr, value) = match trigger {
            Trigger::Exec { addr } => (HookKind::Breakpoint, addr, 0),
            Trigger::Write { addr, size } => {
                // The store has completed by the time the emulator stops.
                let bytes = read_bytes(
                    &mut self.emu,
                    cpu,
                    MemRange {
                        addr,
                        len: size as u32,
                    },
                );
                let value = bytes
                    .iter()
                    .rev()
                    .fold(0, |value, &byte| value << 8 | byte as u32);
                (HookKind::WriteWatchpoint, addr, value)
            }
        };
        let matching: Vec<_> = self
            .hooks
            .hooks
            .iter()
            .filter(|hook| {
                hook.kind == kind
                    && hook.cpu == cpu
                    && match trigger {
                        Trigger::Exec { addr } => hook.addr == addr,
                        Trigger::Write { addr, size } => {
                            overlaps(hook.addr, hook.size, addr, size as u32)
                        }
                    }
            })
            .map(|hook| (hook.id, hook.reads.clone()))
            .collect();
        matching
            .into_iter()
            .map(|(id, reads)| HookHit {
                id,
                kind,
                cpu,
                addr,
                value,
                frame: self.frame,
                regs,
                reads: reads
                    .into_iter()
                    .map(|range| read_bytes(&mut self.emu, cpu, range))
                    .collect(),
            })
            .collect()
    }

    fn handle_stop(&mut self) -> Result<(), Error> {
        let mut hits = Vec::new();
        let mut exec_addrs = [None; 2];
        for cpu in [Cpu::Arm9, Cpu::Arm7] {
            let triggers =
                std::mem::take(&mut self.hooks.shared.borrow_mut().triggers[cpu as usize]);
            for trigger in triggers {
                if let Trigger::Exec { addr } = trigger {
                    exec_addrs[cpu as usize] = Some(addr);
                }
                hits.extend(self.hits_for(cpu, trigger));
            }
        }

        let mut result = Ok(());
        if let Some(mut handler) = self.hooks.handler.take() {
            for hit in hits {
                self.hooks.current = Some(hit.clone());
                self.hooks.continue_requested = false;
                let flow = handler(self, &hit);
                self.hooks.current = None;
                self.hooks.continue_requested = false;
                if flow.is_break() {
                    result = Err(Error::Interrupted);
                    break;
                }
            }
            // The handler may have set another one.
            self.hooks.handler.get_or_insert(handler);
        }

        // Resume the stopped cores; one stopped at a breakpoint skips the check of its next
        // instruction so it doesn't hit the same breakpoint again, unless the handler moved the
        // PC. The core clears the skip once any instruction runs, so an IRQ taken on resume
        // can't eat a later hit.
        let pcs = [next_pc(&self.emu, Cpu::Arm9), next_pc(&self.emu, Cpu::Arm7)];
        for (cpu, is_stopped, skip) in [
            (
                Cpu::Arm9,
                &mut self.emu.arm9.is_stopped,
                &mut self.emu.arm9.skip_next_breakpoint,
            ),
            (
                Cpu::Arm7,
                &mut self.emu.arm7.is_stopped,
                &mut self.emu.arm7.skip_next_breakpoint,
            ),
        ] {
            if std::mem::take(is_stopped) {
                *skip = exec_addrs[cpu as usize] == Some(pcs[cpu as usize]);
            }
        }
        result
    }
}
