//! Memory and CPU register access, and watched memory ranges.
//!
//! Main RAM (0x0200_0000-0x02FF_FFFF) is accessed directly, with its mirror mask; every other
//! address goes through the chosen CPU's bus with debug accesses, so it sees that CPU's memory map.

use crate::Error;
use dust_core::{
    cpu::{arm7, arm9, bus::DebugCpuAccess, interpreter::Interpreter, psr::Psr},
    emu::Emu,
};

/// Largest single read, write or watch, in bytes.
pub const MAX_LEN: u32 = 16 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cpu {
    #[default]
    Arm9,
    Arm7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regs {
    /// r0-r15, as the CPU holds them: r15 reads ahead of the executing instruction (+8 in ARM
    /// state, +4 in Thumb state).
    pub gprs: [u32; 16],
    pub cpsr: u32,
}

/// Register index for [`crate::Session::set_reg`]: 0-15 are r0-r15, 16 is cpsr.
pub const REG_PC: u32 = 15;
pub const REG_CPSR: u32 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchChange {
    pub watch_id: u32,
    pub addr: u32,
    pub old_data: Vec<u8>,
    pub new_data: Vec<u8>,
}

struct Watch {
    id: u32,
    cpu: Cpu,
    addr: u32,
    data: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Watches {
    list: Vec<Watch>,
    next_id: u32,
}

impl Watches {
    pub fn add(
        &mut self,
        emu: &mut Emu<Interpreter>,
        cpu: Cpu,
        addr: u32,
        len: u32,
    ) -> Result<u32, Error> {
        let data = read(emu, cpu, addr, len)?;
        self.next_id += 1;
        self.list.push(Watch {
            id: self.next_id,
            cpu,
            addr,
            data,
        });
        Ok(self.next_id)
    }

    pub fn remove(&mut self, id: u32) -> Result<(), Error> {
        let len = self.list.len();
        self.list.retain(|watch| watch.id != id);
        if self.list.len() == len {
            return Err(Error::InvalidArgument(format!("no watch with id {id}")));
        }
        Ok(())
    }

    /// Rereads every watched range and returns the ones that changed since the last call.
    pub fn changes(&mut self, emu: &mut Emu<Interpreter>) -> Vec<WatchChange> {
        let mut changes = Vec::new();
        for watch in &mut self.list {
            let new_data = read_unchecked(emu, watch.cpu, watch.addr, watch.data.len() as u32);
            if new_data != watch.data {
                changes.push(WatchChange {
                    watch_id: watch.id,
                    addr: watch.addr,
                    old_data: std::mem::replace(&mut watch.data, new_data.clone()),
                    new_data,
                });
            }
        }
        changes
    }
}

fn check_len(len: usize) -> Result<(), Error> {
    if len > MAX_LEN as usize {
        return Err(Error::InvalidArgument(format!(
            "{len} B is more than the {MAX_LEN} B limit"
        )));
    }
    Ok(())
}

fn is_main_ram(addr: u32) -> bool {
    addr >> 24 == 0x02
}

fn main_ram_ptr(emu: &Emu<Interpreter>, addr: u32) -> *mut u8 {
    // In bounds: the mask is at most 0x7F_FFFF and main RAM is 0x80_0000 B.
    unsafe {
        emu.main_mem()
            .as_mut_ptr()
            .add((addr & emu.main_mem_mask().get()) as usize)
    }
}

fn read_8(emu: &mut Emu<Interpreter>, cpu: Cpu, addr: u32) -> u8 {
    if is_main_ram(addr) {
        return unsafe { main_ram_ptr(emu, addr).read() };
    }
    match cpu {
        Cpu::Arm9 => arm9::bus::read_8::<DebugCpuAccess, _>(emu, addr),
        Cpu::Arm7 => arm7::bus::read_8::<DebugCpuAccess, _>(emu, addr),
    }
}

fn write_8(emu: &mut Emu<Interpreter>, cpu: Cpu, addr: u32, value: u8) {
    if is_main_ram(addr) {
        return unsafe { main_ram_ptr(emu, addr).write(value) };
    }
    match cpu {
        Cpu::Arm9 => arm9::bus::write_8::<DebugCpuAccess, _>(emu, addr, value),
        Cpu::Arm7 => arm7::bus::write_8::<DebugCpuAccess, _>(emu, addr, value),
    }
}

pub(crate) fn read_unchecked(emu: &mut Emu<Interpreter>, cpu: Cpu, addr: u32, len: u32) -> Vec<u8> {
    (0..len)
        .map(|i| read_8(emu, cpu, addr.wrapping_add(i)))
        .collect()
}

pub(crate) fn read(
    emu: &mut Emu<Interpreter>,
    cpu: Cpu,
    addr: u32,
    len: u32,
) -> Result<Vec<u8>, Error> {
    check_len(len as usize)?;
    Ok(read_unchecked(emu, cpu, addr, len))
}

pub(crate) fn write(
    emu: &mut Emu<Interpreter>,
    cpu: Cpu,
    addr: u32,
    data: &[u8],
) -> Result<(), Error> {
    check_len(data.len())?;
    for (i, &byte) in data.iter().enumerate() {
        write_8(emu, cpu, addr.wrapping_add(i as u32), byte);
    }
    Ok(())
}

/// Writes `value` as one 1, 2 or 4-byte access (little-endian), so I/O registers see a single
/// write of that width.
pub(crate) fn write_value(
    emu: &mut Emu<Interpreter>,
    cpu: Cpu,
    addr: u32,
    value: u32,
    size: u32,
) -> Result<(), Error> {
    match (size, cpu) {
        _ if is_main_ram(addr) && matches!(size, 1 | 2 | 4) => {
            write(emu, cpu, addr, &value.to_le_bytes()[..size as usize])?
        }
        (1, _) => write_8(emu, cpu, addr, value as u8),
        (2, Cpu::Arm9) => arm9::bus::write_16::<DebugCpuAccess, _>(emu, addr, value as u16),
        (2, Cpu::Arm7) => arm7::bus::write_16::<DebugCpuAccess, _>(emu, addr, value as u16),
        (4, Cpu::Arm9) => arm9::bus::write_32::<DebugCpuAccess, _>(emu, addr, value),
        (4, Cpu::Arm7) => arm7::bus::write_32::<DebugCpuAccess, _>(emu, addr, value),
        _ => {
            return Err(Error::InvalidArgument(format!(
                "write size must be 1, 2 or 4, not {size}"
            )))
        }
    }
    Ok(())
}

pub(crate) fn regs(emu: &Emu<Interpreter>, cpu: Cpu) -> Regs {
    let (regs, cpsr) = match cpu {
        Cpu::Arm9 => (emu.arm9.regs(), emu.arm9.cpsr()),
        Cpu::Arm7 => (emu.arm7.regs(), emu.arm7.cpsr()),
    };
    Regs {
        gprs: regs.gprs,
        cpsr: cpsr.raw(),
    }
}

/// Writing r15 jumps to `value`, the next instruction to execute (keeping the ARM/Thumb state);
/// writing cpsr keeps its reserved bits clear.
pub(crate) fn set_reg(
    emu: &mut Emu<Interpreter>,
    cpu: Cpu,
    reg: u32,
    value: u32,
) -> Result<(), Error> {
    match (reg, cpu) {
        (0..=14, Cpu::Arm9) => {
            let mut regs = emu.arm9.regs();
            regs.gprs[reg as usize] = value;
            arm9::Arm9::set_regs(emu, &regs);
        }
        (0..=14, Cpu::Arm7) => {
            let mut regs = emu.arm7.regs();
            regs.gprs[reg as usize] = value;
            arm7::Arm7::set_regs(emu, &regs);
        }
        (REG_PC, Cpu::Arm9) => {
            let thumb = emu.arm9.cpsr().thumb_state() as u32;
            arm9::Arm9::jump(emu, (value & !1) | thumb);
        }
        (REG_PC, Cpu::Arm7) => {
            let thumb = emu.arm7.cpsr().thumb_state() as u32;
            arm7::Arm7::jump(emu, (value & !1) | thumb);
        }
        (REG_CPSR, Cpu::Arm9) => arm9::Arm9::set_cpsr(emu, Psr::from_raw_masked::<true>(value)),
        (REG_CPSR, Cpu::Arm7) => arm7::Arm7::set_cpsr(emu, Psr::from_raw_masked::<false>(value)),
        _ => {
            return Err(Error::InvalidArgument(format!(
                "register must be 0-15 (r0-r15) or 16 (cpsr), not {reg}"
            )))
        }
    }
    Ok(())
}
