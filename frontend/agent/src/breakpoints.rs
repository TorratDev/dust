//! Exec breakpoints and write watchpoints, on top of dust-core's debugger hooks.
//!
//! A hit stops the emulator mid-frame, right before the breakpoint's instruction executes or right
//! after the watched write. [`Session::step`] then calls the session's hit handler (see
//! [`Session::set_hit_handler`]) with the emulator still stopped, and resumes once it returns; the
//! hits are also returned in [`crate::StepResult::hits`].
//!
//! Addresses are matched as the CPU issues them: main RAM mirrors are different addresses.

use crate::{
    memory::{self, Cpu, Regs, MAX_LEN},
    Error,
};
use dust_core::{
    cpu::{
        debug::{
            BreakpointHook, MemWatchpointHook, MemWatchpointRwMask, MemWatchpointTriggerCause,
            MWLT_BYTES_PER_ENTRY_SHIFT,
        },
        interpreter::Interpreter,
    },
    emu::Emu,
};
use std::{cell::RefCell, rc::Rc};

/// Most reads a single hook can register.
pub const MAX_HOOK_READS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookKind {
    Breakpoint,
    WriteWatchpoint,
}

/// A memory range read on every hit of a hook, so handlers don't need another round trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemRange {
    pub addr: u32,
    pub len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub id: u32,
    pub kind: HookKind,
    pub cpu: Cpu,
    /// Instruction address for breakpoints, first watched byte for watchpoints.
    pub addr: u32,
    /// Watched bytes; 0 for breakpoints.
    pub size: u32,
    /// Read with the hook's CPU on every hit.
    pub reads: Vec<MemRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookHit {
    pub id: u32,
    pub kind: HookKind,
    pub cpu: Cpu,
    /// Executed address for breakpoints; written address (aligned to the access size) for
    /// watchpoints.
    pub addr: u32,
    /// Watchpoints: the value written by the access (1, 2 or 4 bytes, little-endian, as read
    /// back right after the instruction). 0 for breakpoints.
    pub value: u32,
    /// Watchpoints: the access size in bytes. 0 for breakpoints.
    pub size: u32,
    /// The frame being emulated (frames finished so far).
    pub frame: u64,
    pub regs: Regs,
    /// Results of the hook's reads, in order.
    pub reads: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
enum RawHit {
    Exec { cpu: Cpu, addr: u32 },
    Write { cpu: Cpu, addr: u32, size: u8 },
}

/// State shared with the closures installed in the emulator.
#[derive(Default)]
struct Shared {
    pending: Vec<RawHit>,
    /// Per CPU (ARM9, ARM7): a breakpoint address to let through once, so resuming at the
    /// instruction a breakpoint stopped on executes it instead of stopping again.
    skip_exec: [Option<u32>; 2],
}

fn cpu_index(cpu: Cpu) -> usize {
    match cpu {
        Cpu::Arm9 => 0,
        Cpu::Arm7 => 1,
    }
}

/// Splits `[addr, addr + size)` into naturally aligned power-of-two chunks that fit in one
/// watchpoint table entry.
fn watch_chunks(addr: u32, size: u32) -> impl Iterator<Item = (u32, u8)> {
    let max_chunk = 1u32 << MWLT_BYTES_PER_ENTRY_SHIFT;
    let end = addr as u64 + size as u64;
    let mut cur = addr as u64;
    std::iter::from_fn(move || {
        if cur >= end {
            return None;
        }
        let align = if cur == 0 {
            max_chunk
        } else {
            (1u32 << (cur as u32).trailing_zeros()).min(max_chunk)
        };
        let mut chunk = align;
        while cur + chunk as u64 > end {
            chunk >>= 1;
        }
        let item = (cur as u32, chunk as u8);
        cur += chunk as u64;
        Some(item)
    })
}

pub(crate) struct Hooks {
    list: Vec<Hook>,
    next_id: u32,
    shared: Rc<RefCell<Shared>>,
}

impl Hooks {
    pub fn new() -> Self {
        Hooks {
            list: Vec::new(),
            next_id: 0,
            shared: Rc::default(),
        }
    }

    pub fn list(&self) -> &[Hook] {
        &self.list
    }

    /// Sets up a (new or rebuilt) emulator: installs the hook closures and every hook.
    pub fn install(&self, emu: &mut Emu<Interpreter>) {
        {
            let mut shared = self.shared.borrow_mut();
            shared.pending.clear();
            shared.skip_exec = [None; 2];
        }
        for cpu in [Cpu::Arm9, Cpu::Arm7] {
            let shared = Rc::clone(&self.shared);
            let breakpoint_hook = BreakpointHook::new(Box::new(move |_emu, addr| {
                let mut shared = shared.borrow_mut();
                let skip = &mut shared.skip_exec[cpu_index(cpu)];
                if *skip == Some(addr) {
                    *skip = None;
                    return false;
                }
                shared.pending.push(RawHit::Exec { cpu, addr });
                true
            }));
            let shared = Rc::clone(&self.shared);
            let watchpoint_hook =
                MemWatchpointHook::new(Box::new(move |_emu, addr, size, cause| {
                    if cause != MemWatchpointTriggerCause::Write {
                        return false;
                    }
                    shared
                        .borrow_mut()
                        .pending
                        .push(RawHit::Write { cpu, addr, size });
                    true
                }));
            match cpu {
                Cpu::Arm9 => {
                    emu.arm9.set_breakpoint_hook(Some(breakpoint_hook));
                    emu.arm9.set_mem_watchpoint_hook(Some(watchpoint_hook));
                }
                Cpu::Arm7 => {
                    emu.arm7.set_breakpoint_hook(Some(breakpoint_hook));
                    emu.arm7.set_mem_watchpoint_hook(Some(watchpoint_hook));
                }
            }
        }
        emu.arm9.clear_breakpoints();
        emu.arm7.clear_breakpoints();
        for hook in &self.list {
            if hook.kind == HookKind::Breakpoint {
                add_core_breakpoint(emu, hook.cpu, hook.addr);
            }
        }
        for cpu in [Cpu::Arm9, Cpu::Arm7] {
            self.reinstall_watchpoints(emu, cpu);
        }
    }

    fn reinstall_watchpoints(&self, emu: &mut Emu<Interpreter>, cpu: Cpu) {
        match cpu {
            Cpu::Arm9 => emu.arm9.clear_mem_watchpoints(),
            Cpu::Arm7 => emu.arm7.clear_mem_watchpoints(),
        }
        for hook in &self.list {
            if hook.kind == HookKind::WriteWatchpoint && hook.cpu == cpu {
                for (addr, size) in watch_chunks(hook.addr, hook.size) {
                    match cpu {
                        Cpu::Arm9 => {
                            emu.arm9
                                .add_mem_watchpoint(addr, size, MemWatchpointRwMask::WRITE)
                        }
                        Cpu::Arm7 => {
                            emu.arm7
                                .add_mem_watchpoint(addr, size, MemWatchpointRwMask::WRITE)
                        }
                    }
                }
            }
        }
    }

    fn check_reads(reads: &[MemRange]) -> Result<(), Error> {
        if reads.len() > MAX_HOOK_READS {
            return Err(Error::InvalidArgument(format!(
                "a hook can read at most {MAX_HOOK_READS} ranges, not {}",
                reads.len()
            )));
        }
        let total: u64 = reads.iter().map(|range| range.len as u64).sum();
        if total > MAX_LEN as u64 {
            return Err(Error::InvalidArgument(format!(
                "a hook's reads total {total} B, more than the {MAX_LEN} B limit"
            )));
        }
        Ok(())
    }

    fn push(&mut self, mut hook: Hook) -> u32 {
        self.next_id += 1;
        hook.id = self.next_id;
        self.list.push(hook);
        self.next_id
    }

    pub fn add_breakpoint(
        &mut self,
        emu: &mut Emu<Interpreter>,
        cpu: Cpu,
        addr: u32,
        reads: Vec<MemRange>,
    ) -> Result<u32, Error> {
        Self::check_reads(&reads)?;
        // Thumb code addresses (bit 0 set) name the same instruction.
        let addr = addr & !1;
        add_core_breakpoint(emu, cpu, addr);
        Ok(self.push(Hook {
            id: 0,
            kind: HookKind::Breakpoint,
            cpu,
            addr,
            size: 0,
            reads,
        }))
    }

    pub fn add_write_watchpoint(
        &mut self,
        emu: &mut Emu<Interpreter>,
        cpu: Cpu,
        addr: u32,
        size: u32,
        reads: Vec<MemRange>,
    ) -> Result<u32, Error> {
        Self::check_reads(&reads)?;
        if size == 0 || size > MAX_LEN {
            return Err(Error::InvalidArgument(format!(
                "watchpoint size must be 1-{MAX_LEN} B, not {size}"
            )));
        }
        if addr.checked_add(size - 1).is_none() {
            return Err(Error::InvalidArgument(format!(
                "watchpoint at {addr:#010x} with size {size} wraps around the address space"
            )));
        }
        let id = self.push(Hook {
            id: 0,
            kind: HookKind::WriteWatchpoint,
            cpu,
            addr,
            size,
            reads,
        });
        for (addr, size) in watch_chunks(addr, size) {
            match cpu {
                Cpu::Arm9 => emu
                    .arm9
                    .add_mem_watchpoint(addr, size, MemWatchpointRwMask::WRITE),
                Cpu::Arm7 => emu
                    .arm7
                    .add_mem_watchpoint(addr, size, MemWatchpointRwMask::WRITE),
            }
        }
        Ok(id)
    }

    pub fn remove(
        &mut self,
        emu: &mut Emu<Interpreter>,
        id: u32,
        kind: HookKind,
    ) -> Result<(), Error> {
        let Some(i) = self
            .list
            .iter()
            .position(|hook| hook.id == id && hook.kind == kind)
        else {
            let what = match kind {
                HookKind::Breakpoint => "breakpoint",
                HookKind::WriteWatchpoint => "write watchpoint",
            };
            return Err(Error::InvalidArgument(format!("no {what} with id {id}")));
        };
        let hook = self.list.remove(i);
        match kind {
            HookKind::Breakpoint => {
                let still_used = self.list.iter().any(|other| {
                    other.kind == HookKind::Breakpoint
                        && other.cpu == hook.cpu
                        && other.addr == hook.addr
                });
                if !still_used {
                    match hook.cpu {
                        Cpu::Arm9 => emu.arm9.remove_breakpoint(hook.addr),
                        Cpu::Arm7 => emu.arm7.remove_breakpoint(hook.addr),
                    }
                }
            }
            // The core's watchpoint table isn't reference counted: rebuild it from the rest.
            HookKind::WriteWatchpoint => self.reinstall_watchpoints(emu, hook.cpu),
        }
        Ok(())
    }

    /// Turns the raw hits recorded since the last call into [`HookHit`]s, and prepares the
    /// emulator to resume: breakpoints that stopped a CPU are let through once.
    pub fn take_hits(&mut self, emu: &mut Emu<Interpreter>, frame: u64) -> Vec<HookHit> {
        let pending = std::mem::take(&mut self.shared.borrow_mut().pending);
        let mut hits = Vec::new();
        for raw in pending {
            let (cpu, kind, addr, size) = match raw {
                RawHit::Exec { cpu, addr } => {
                    self.shared.borrow_mut().skip_exec[cpu_index(cpu)] = Some(addr);
                    (cpu, HookKind::Breakpoint, addr, 0)
                }
                RawHit::Write { cpu, addr, size } => {
                    (cpu, HookKind::WriteWatchpoint, addr, size as u32)
                }
            };
            let value = if kind == HookKind::WriteWatchpoint {
                let bytes = memory::read(emu, cpu, addr, size).unwrap_or_default();
                let mut le = [0; 4];
                le[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
                u32::from_le_bytes(le)
            } else {
                0
            };
            let regs = memory::regs(emu, cpu);
            for hook in &self.list {
                let matches = hook.cpu == cpu
                    && hook.kind == kind
                    && match kind {
                        HookKind::Breakpoint => hook.addr == addr,
                        HookKind::WriteWatchpoint => {
                            (addr as u64) < hook.addr as u64 + hook.size as u64
                                && (hook.addr as u64) < addr as u64 + size as u64
                        }
                    };
                if !matches {
                    continue;
                }
                let reads = hook
                    .reads
                    .iter()
                    .map(|range| memory::read(emu, cpu, range.addr, range.len).unwrap_or_default())
                    .collect();
                hits.push(HookHit {
                    id: hook.id,
                    kind,
                    cpu,
                    addr,
                    value,
                    size,
                    frame,
                    regs,
                    reads,
                });
            }
        }
        hits
    }
}

fn add_core_breakpoint(emu: &mut Emu<Interpreter>, cpu: Cpu, addr: u32) {
    match cpu {
        Cpu::Arm9 => emu.arm9.add_breakpoint(addr),
        Cpu::Arm7 => emu.arm7.add_breakpoint(addr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_range() {
        for (addr, size) in [(0, 1), (1, 7), (0x0200_0003, 70), (0xFFFF_FFF0, 16), (5, 3)] {
            let chunks: Vec<_> = watch_chunks(addr, size).collect();
            let mut next = addr as u64;
            for &(chunk_addr, chunk_size) in &chunks {
                assert_eq!(chunk_addr as u64, next);
                assert!(chunk_size.is_power_of_two());
                assert_eq!(chunk_addr % chunk_size as u32, 0);
                assert!(chunk_size as u32 <= 1 << MWLT_BYTES_PER_ENTRY_SHIFT);
                next += chunk_size as u64;
            }
            assert_eq!(next, addr as u64 + size as u64);
        }
    }
}
