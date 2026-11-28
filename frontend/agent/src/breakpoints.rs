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

use crate::{Error, Session};
use dust_core::{
    cpu::{
        arm7, arm9,
        bus::DebugCpuAccess,
        debug::{
            BreakpointHook, MemWatchpointHook, MemWatchpointRwMask, MemWatchpointTriggerCause,
        },
        interpreter::Interpreter,
    },
    emu::{self, RunOutput},
    utils::schedule::RawTimestamp,
};
use std::{cell::RefCell, ops::ControlFlow, rc::Rc};

/// Largest write watchpoint, in bytes.
pub const MAX_WATCH_SIZE: u32 = 0x1_0000;
/// Largest total length of the reads registered with one hook, in bytes.
pub const MAX_HOOK_READ_LEN: u32 = 0x10_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Cpu {
    #[default]
    Arm9,
    Arm7,
}

impl Cpu {
    fn index(self) -> usize {
        self as usize
    }
}

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuRegs {
    /// r0-r15 of the current mode; r15 is the address of the next instruction to execute.
    pub gprs: [u32; 16],
    pub cpsr: u32,
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
    pub regs: CpuRegs,
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
    /// Breakpoint address each core is resuming from; its next check there is skipped once so
    /// continuing doesn't hit the same breakpoint again.
    skip: [Option<u32>; 2],
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
                let mut shared = shared.borrow_mut();
                if shared.skip[$cpu.index()].take() == Some(addr) {
                    return false;
                }
                shared.triggers[$cpu.index()].push(Trigger::Exec { addr });
                true
            }))));
        let shared = Rc::clone($shared);
        $emu.$core
            .set_mem_watchpoint_hook(Some(MemWatchpointHook::new(Box::new(
                move |_emu, addr, size, cause| {
                    if cause != MemWatchpointTriggerCause::Write {
                        return false;
                    }
                    shared.borrow_mut().triggers[$cpu.index()].push(Trigger::Write { addr, size });
                    true
                },
            ))));
    }};
}

fn read_bytes(emu: &mut emu::Emu<Interpreter>, cpu: Cpu, range: MemRange) -> Vec<u8> {
    (0..range.len)
        .map(|i| {
            let addr = range.addr.wrapping_add(i);
            match cpu {
                Cpu::Arm9 => arm9::bus::read_8::<DebugCpuAccess, _>(emu, addr),
                Cpu::Arm7 => arm7::bus::read_8::<DebugCpuAccess, _>(emu, addr),
            }
        })
        .collect()
}

fn regs(emu: &emu::Emu<Interpreter>, cpu: Cpu) -> CpuRegs {
    let (regs, cpsr) = match cpu {
        Cpu::Arm9 => (emu.arm9.regs(), emu.arm9.cpsr()),
        Cpu::Arm7 => (emu.arm7.regs(), emu.arm7.cpsr()),
    };
    let mut gprs = regs.gprs;
    // r15 reads two instructions ahead (pipeline); report the next instruction's address.
    gprs[15] = gprs[15].wrapping_sub(8 >> cpsr.thumb_state() as u8);
    CpuRegs {
        gprs,
        cpsr: cpsr.raw(),
    }
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

    /// Stops after `cpu` stores to any of the `size` bytes from `addr`. DMA transfers don't
    /// trigger watchpoints. Returns the hook id.
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
                    let mut shared = self.hooks.shared.borrow_mut();
                    let skip = &mut shared.skip[hook.cpu.index()];
                    if *skip == Some(hook.addr) {
                        *skip = None;
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
        loop {
            // Effectively unlimited cycles: unlike `Emu::run`, this resumes mid-frame correctly
            // after a stop.
            let mut cycles: [RawTimestamp; 2] = [1 << 60; 2];
            match self.emu.run_with_cycles(&mut cycles) {
                RunOutput::FrameFinished => return Ok(()),
                RunOutput::Shutdown => return Err(Error::Shutdown),
                RunOutput::StoppedByDebugHook => self.handle_stop()?,
                RunOutput::CyclesOver(_) => {}
            }
        }
    }

    fn hits_for(&mut self, cpu: Cpu, trigger: Trigger) -> Vec<HookHit> {
        let regs = regs(&self.emu, cpu);
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
                std::mem::take(&mut self.hooks.shared.borrow_mut().triggers[cpu.index()]);
            for trigger in triggers {
                if let Trigger::Exec { addr } = trigger {
                    exec_addrs[cpu.index()] = Some(addr);
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

        // Resume the stopped cores; one stopped at a breakpoint must not hit it again right away.
        // (A hook removed by the handler leaves no skip behind: removing clears it.)
        let mut shared = self.hooks.shared.borrow_mut();
        for (cpu, is_stopped) in [
            (Cpu::Arm9, &mut self.emu.arm9.is_stopped),
            (Cpu::Arm7, &mut self.emu.arm7.is_stopped),
        ] {
            if std::mem::take(is_stopped) {
                let still_set = |addr: &u32| {
                    self.hooks.hooks.iter().any(|hook| {
                        hook.kind == HookKind::Breakpoint && hook.cpu == cpu && hook.addr == *addr
                    })
                };
                shared.skip[cpu.index()] = exec_addrs[cpu.index()].filter(still_set);
            }
        }
        result
    }
}
