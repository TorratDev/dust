#[inline]
fn arm_shift_ty_to_str(shift_ty: arm_decoder::arm::ShiftTy) -> &'static str {
    [
        "ShiftTy::Lsl",
        "ShiftTy::Lsr",
        "ShiftTy::Asr",
        "ShiftTy::Ror",
    ][shift_ty as usize]
}

use arm_decoder::{arm, thumb, Processor};
use std::{
    env,
    fs::File,
    io::{self, BufWriter, Write},
};

mod interpreter {
    use super::*;

    fn output_arm_cond_instr_table(
        filename: &str,
        table: &[arm::Instr],
        is_arm9: bool,
    ) -> Result<(), io::Error> {
        use arm::{
            DpOperand, DspMulTy, Instr, MiscAddressing, MiscTransferTy, WbAddressing, WbOff,
        };
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        let mut key = 0_u16;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::Mrs { spsr } => write!(file, "mrs::<{spsr}>"),
                    Instr::Msr { ty, spsr } => {
                        write!(file, "msr::<{}, {spsr}>", ty != arm::MsrTy::Reg)
                    }
                    Instr::Bx { link } => {
                        if is_arm9 {
                            write!(file, "bx::<{link}>")
                        } else {
                            write!(file, "bx")
                        }
                    }
                    Instr::Clz => write!(file, "clz"),
                    Instr::SatAddSub { sub, doubled } => {
                        write!(file, "qaddsub::<{sub}, {doubled}>")
                    }
                    Instr::Bkpt => write!(file, "bkpt"),
                    Instr::DspMul(ty) => {
                        if is_arm9 {
                            match ty {
                                DspMulTy::Smulxy { acc } => write!(file, "smulxy::<{acc}>"),
                                DspMulTy::Smulwy { acc } => write!(file, "smulwy::<{acc}>"),
                                DspMulTy::Smlalxy => write!(file, "smlalxy"),
                            }
                        } else {
                            write!(file, "nop")
                        }
                    }
                    Instr::DpOp { ty, set_flags, op } => {
                        write!(
                            file,
                            "dp_op::<{{DpOpTy::{} as u8}}, ",
                            [
                                "And", "Eor", "Sub", "Rsb", "Add", "Adc", "Sbc", "Rsc", "Tst",
                                "Teq", "Cmp", "Cmn", "Orr", "Mov", "Bic", "Mvn",
                            ][ty as usize]
                        )?;
                        match op {
                            DpOperand::Imm => write!(file, "{{DpOperand::IMM}}"),
                            DpOperand::Reg {
                                shift_ty,
                                shift_imm,
                            } => write!(
                                file,
                                "{{DpOperand::encode_reg({}, {})}}",
                                arm_shift_ty_to_str(shift_ty),
                                shift_imm
                            ),
                        }?;
                        write!(file, ", {set_flags}>")
                    }
                    Instr::Mul { acc, set_flags } => {
                        write!(file, "mul::<{acc}, {set_flags}>")
                    }
                    Instr::MulLong {
                        acc,
                        set_flags,
                        signed,
                    } => write!(
                        file,
                        "{}mull::<{}, {}>",
                        if signed { 's' } else { 'u' },
                        acc,
                        set_flags
                    ),
                    Instr::Swp { byte } => {
                        write!(file, "swp{}", if byte { "b" } else { "" })
                    }
                    Instr::MiscTransfer {
                        ty,
                        addressing,
                        offset_upwards,
                        offset_imm,
                    } => {
                        write!(
                            file,
                            "{}::<{}, {}, {{MiscAddressing::{} as u8}}>",
                            match ty {
                                MiscTransferTy::Ldrh => "ldrh",
                                MiscTransferTy::Strh => "strh",
                                MiscTransferTy::Ldrd => "ldrd",
                                MiscTransferTy::Strd => "strd",
                                MiscTransferTy::Ldrsb => "ldrsb",
                                MiscTransferTy::Ldrsh => "ldrsh",
                            },
                            offset_imm,
                            offset_upwards,
                            match addressing {
                                MiscAddressing::Post => "Post",
                                MiscAddressing::PreNoWb => "PreNoWb",
                                MiscAddressing::Pre => "Pre",
                            }
                        )
                    }
                    Instr::WbTransfer {
                        load,
                        byte,
                        addressing,
                        offset_upwards,
                        offset,
                    } => {
                        let load_store = if load { "ldr" } else { "str" };
                        let word_byte = if byte { "b" } else { "" };
                        let addressing = match addressing {
                            WbAddressing::Post => "Post",
                            WbAddressing::PostUser => "PostUser",
                            WbAddressing::PreNoWb => "PreNoWb",
                            WbAddressing::Pre => "Pre",
                        };
                        match offset {
                            WbOff::Reg(shift_ty) => write!(
                                file,
                                "{}{}::<{{WbOffTy::encode_reg({})}}, {}, {{WbAddressing::{} as u8}}>",
                                load_store,
                                word_byte,
                                arm_shift_ty_to_str(shift_ty),
                                offset_upwards,
                                addressing
                            ),
                            WbOff::Imm => write!(
                                file,
                                "{load_store}{word_byte}::<{{WbOffTy::IMM}}, {offset_upwards}, \
                                 {{WbAddressing::{addressing} as u8}}>"
                            ),
                        }
                    }
                    Instr::TransferMultiple {
                        load,
                        increment,
                        base_excluded,
                        writeback,
                        s_bit,
                    } => {
                        let preinc = base_excluded ^ !increment;
                        write!(
                            file,
                            "{}::<{}, {}, {}, {}>",
                            if load { "ldm" } else { "stm" },
                            increment,
                            preinc,
                            writeback,
                            s_bit
                        )
                    }
                    Instr::Branch { link } => write!(file, "b::<{link}>"),
                    Instr::Mcrr => write!(file, "mcrr"),
                    Instr::Mrrc => write!(file, "mrrc"),
                    Instr::Stc => write!(file, "stc"),
                    Instr::Ldc => write!(file, "ldc"),
                    Instr::Cdp => write!(file, "cdp"),
                    Instr::Mcr => write!(file, "mcr"),
                    Instr::Mrc => write!(file, "mrc"),
                    Instr::Swi => write!(file, "swi"),
                    Instr::Undefined { .. } => {
                        write!(
                            file,
                            "undefined::<{}, {}>",
                            !is_arm9 && key == 0x605,
                            key == 0x601,
                        )
                    }
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
                key += 1;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_arm_uncond_instr_table(
        filename: &str,
        table: &[arm::UncondInstr],
    ) -> Result<(), io::Error> {
        use arm::UncondInstr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        let mut key = 0_u16;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    UncondInstr::Pld => write!(file, "pld"),
                    UncondInstr::BlxImm => write!(file, "blx_imm"),
                    UncondInstr::Mcrr => write!(file, "mcrr"),
                    UncondInstr::Mrrc => write!(file, "mrrc"),
                    UncondInstr::Stc => write!(file, "stc"),
                    UncondInstr::Ldc => write!(file, "ldc"),
                    UncondInstr::Cdp => write!(file, "cdp"),
                    UncondInstr::Mcr => write!(file, "mcr"),
                    UncondInstr::Mrc => write!(file, "mrc"),
                    UncondInstr::Undefined => write!(file, "undefined::<{}, false>", key == 0xF05),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
                key += 1;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_thumb_instr_table(
        filename: &str,
        table: &[thumb::Instr],
        is_arm9: bool,
    ) -> Result<(), io::Error> {
        use thumb::Instr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::AddSubRegImm3 { sub, imm3, op } => {
                        write!(
                            file,
                            "add_sub_reg_imm3::<{}, {}, {}>",
                            sub,
                            imm3,
                            imm3 && op == 0
                        )
                    }
                    Instr::ShiftImm { ty, shift: _ } => {
                        write!(
                            file,
                            "shift_imm::<{{ShiftImmTy::{} as u8}}>",
                            ["Lsl", "Lsr", "Asr"][ty as usize]
                        )
                    }
                    Instr::DpOpImm8 { ty, reg: _ } => {
                        write!(
                            file,
                            "dp_op_imm8::<{{DpOpImm8Ty::{} as u8}}>",
                            ["Mov", "Cmp", "Add", "Sub"][ty as usize]
                        )
                    }
                    Instr::DpOpReg(ty) => {
                        write!(
                            file,
                            "dp_op_reg::<{{DpOpRegTy::{} as u8}}>",
                            [
                                "And", "Eor", "Lsl", "Lsr", "Asr", "Adc", "Sbc", "Ror", "Tst",
                                "Neg", "Cmp", "Cmn", "Orr", "Mul", "Bic", "Mvn",
                            ][ty as usize],
                        )
                    }
                    Instr::Bx {
                        link,
                        addr_high_reg: _,
                    } => {
                        if is_arm9 {
                            write!(file, "bx::<{link}>")
                        } else {
                            write!(file, "bx")
                        }
                    }
                    Instr::DpOpSpecial { ty, h1: _, h2: _ } => {
                        write!(file, "{}_special", ["add", "cmp", "mov"][ty as usize])
                    }
                    Instr::LoadPcRel { reg: _ } => {
                        write!(file, "ldr_pc_rel")
                    }
                    Instr::LoadStoreReg { ty, offset_reg: _ } => {
                        let name = [
                            "str", "strh", "strb", "ldrsb", "ldr", "ldrh", "ldrb", "ldrsh",
                        ][ty as usize];
                        let has_imm_form = ty as u8 & 3 != 3;
                        write!(
                            file,
                            "{}{}",
                            name,
                            if has_imm_form { "::<false>" } else { "" }
                        )
                    }
                    Instr::LoadStoreWbImm {
                        byte,
                        load,
                        offset: _,
                    } => {
                        write!(
                            file,
                            "{}{}::<true>",
                            if load { "ldr" } else { "str" },
                            if byte { "b" } else { "" },
                        )
                    }
                    Instr::LoadStoreHalfImm { load, offset: _ } => {
                        write!(file, "{}h::<true>", if load { "ldr" } else { "str" })
                    }
                    Instr::LoadStoreStack { load, reg: _ } => {
                        write!(file, "{}_sp_rel", if load { "ldr" } else { "str" })
                    }
                    Instr::AddSpPcImm { sp, dst_reg: _ } => {
                        write!(file, "add_pc_sp_imm8::<{sp}>")
                    }
                    Instr::AddSubSpImm7 { sub } => {
                        write!(file, "add_sub_sp_imm7::<{sub}>")
                    }
                    Instr::PushPop {
                        pop,
                        push_r14_pop_r15,
                    } => {
                        write!(
                            file,
                            "{}::<{}>",
                            if pop { "pop" } else { "push" },
                            push_r14_pop_r15
                        )
                    }
                    Instr::Bkpt => {
                        write!(file, "bkpt")
                    }
                    Instr::LoadStoreMultiple { load, base_reg: _ } => {
                        write!(file, "{}ia", if load { "ldm" } else { "stm" })
                    }
                    Instr::Swi => write!(file, "swi"),
                    Instr::CondBranch { cond } => write!(file, "b_cond::<{cond}>"),
                    Instr::Branch => write!(file, "b"),
                    Instr::BlPrefix => write!(file, "bl_prefix"),
                    Instr::BlSuffix { exchange } => {
                        if is_arm9 {
                            write!(file, "bl_suffix::<{exchange}>")
                        } else {
                            write!(file, "bl_suffix")
                        }
                    }
                    Instr::Undefined { .. } => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    pub fn output_instr_tables() {
        output_arm_cond_instr_table("interp_arm7_arm.rs", &arm::cond(Processor::Arm7Tdmi), false)
            .expect("couldn't write interpreter ARM7 ARM instruction table");
        output_thumb_instr_table(
            "interp_arm7_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm7Tdmi),
            false,
        )
        .expect("couldn't write interpreter ARM7 thumb instruction table");

        output_arm_cond_instr_table(
            "interp_arm9_arm_cond.rs",
            &arm::cond(Processor::Arm9Es),
            true,
        )
        .expect("couldn't write interpreter ARM9 ARM cond instruction table");
        output_arm_uncond_instr_table("interp_arm9_arm_uncond.rs", &arm::uncond(Processor::Arm9Es))
            .expect("couldn't write interpreter ARM9 ARM uncond instruction table");
        output_thumb_instr_table(
            "interp_arm9_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm9Es),
            true,
        )
        .expect("couldn't write interpreter ARM9 thumb instruction table");
    }
}

#[cfg(feature = "jit")]
mod jit {
    use super::*;

    fn output_arm_cond_instr_table(
        filename: &str,
        table: &[arm::Instr],
        is_arm9: bool,
    ) -> Result<(), io::Error> {
        use arm::{
            DpOperand, DspMulTy, Instr, MiscAddressing, MiscTransferTy, WbAddressing, WbOff,
        };
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::Mrs { spsr } => write!(file, "mrs::<_, {spsr}>"),
                    Instr::Msr { ty, spsr } => {
                        write!(file, "msr::<_, {}, {spsr}>", ty != arm::MsrTy::Reg)
                    }
                    Instr::Bx { link } => write!(file, "bx::<_, {link}>"),
                    Instr::Clz => write!(file, "clz"),
                    Instr::SatAddSub { sub, doubled } => {
                        write!(file, "qaddsub::<_, {sub}, {doubled}>")
                    }
                    Instr::Bkpt => write!(file, "bkpt"),
                    Instr::DspMul(ty) => {
                        if is_arm9 {
                            match ty {
                                DspMulTy::Smulxy { acc } => write!(file, "smulxy::<_, {acc}>"),
                                DspMulTy::Smulwy { acc } => write!(file, "smulwy::<_, {acc}>"),
                                DspMulTy::Smlalxy => write!(file, "smlalxy"),
                            }
                        } else {
                            write!(file, "nop")
                        }
                    }
                    Instr::DpOp { ty, set_flags, op } => {
                        write!(
                            file,
                            "dp_op::<_, {{DpOpTy::{} as u8}}, ",
                            [
                                "And", "Eor", "Sub", "Rsb", "Add", "Adc", "Sbc", "Rsc", "Tst",
                                "Teq", "Cmp", "Cmn", "Orr", "Mov", "Bic", "Mvn",
                            ][ty as usize]
                        )?;
                        match op {
                            DpOperand::Imm => write!(file, "{{DpOperand::IMM}}"),
                            DpOperand::Reg {
                                shift_ty,
                                shift_imm,
                            } => write!(
                                file,
                                "{{DpOperand::encode_reg({}, {})}}",
                                arm_shift_ty_to_str(shift_ty),
                                shift_imm
                            ),
                        }?;
                        write!(file, ", {set_flags}>")
                    }
                    Instr::Mul { acc, set_flags } => {
                        write!(file, "mul::<_, {acc}, {set_flags}>")
                    }
                    Instr::MulLong {
                        acc,
                        set_flags,
                        signed,
                    } => write!(
                        file,
                        "{}mull::<_, {}, {}>",
                        if signed { 's' } else { 'u' },
                        acc,
                        set_flags
                    ),
                    Instr::Swp { byte } => write!(file, "swp{}", if byte { "b" } else { "" }),
                    Instr::MiscTransfer {
                        ty,
                        addressing,
                        offset_upwards,
                        offset_imm,
                    } => {
                        write!(
                            file,
                            "{}::<_, {}, {}, {{MiscAddressing::{} as u8}}>",
                            match ty {
                                MiscTransferTy::Ldrh => "ldrh",
                                MiscTransferTy::Strh => "strh",
                                MiscTransferTy::Ldrd => "ldrd",
                                MiscTransferTy::Strd => "strd",
                                MiscTransferTy::Ldrsb => "ldrsb",
                                MiscTransferTy::Ldrsh => "ldrsh",
                            },
                            offset_imm,
                            offset_upwards,
                            match addressing {
                                MiscAddressing::Post => "Post",
                                MiscAddressing::PreNoWb => "PreNoWb",
                                MiscAddressing::Pre => "Pre",
                            }
                        )
                    }
                    Instr::WbTransfer {
                        load,
                        byte,
                        addressing,
                        offset_upwards,
                        offset,
                    } => {
                        let load_store = if load { "ldr" } else { "str" };
                        let word_byte = if byte { "b" } else { "" };
                        let addressing = match addressing {
                            WbAddressing::Post => "Post",
                            WbAddressing::PostUser => "PostUser",
                            WbAddressing::PreNoWb => "PreNoWb",
                            WbAddressing::Pre => "Pre",
                        };
                        match offset {
                            WbOff::Reg(shift_ty) => write!(
                                file,
                                "{}{}::<_, {{WbOffTy::encode_reg({})}}, {}, {{WbAddressing::{} as u8}}>",
                                load_store,
                                word_byte,
                                arm_shift_ty_to_str(shift_ty),
                                offset_upwards,
                                addressing
                            ),
                            WbOff::Imm => write!(
                                file,
                                "{}{}::<_, {{WbOffTy::IMM}}, {}, {{WbAddressing::{} as u8}}>",
                                load_store, word_byte, offset_upwards, addressing
                            ),
                        }
                    }
                    Instr::TransferMultiple {
                        load,
                        increment,
                        base_excluded,
                        writeback,
                        s_bit,
                    } => {
                        let preinc = base_excluded ^ !increment;
                        write!(
                            file,
                            "{}::<_, {}, {}, {}, {}>",
                            if load { "ldm" } else { "stm" },
                            increment,
                            preinc,
                            writeback,
                            s_bit
                        )
                    }
                    Instr::Branch { link } => write!(file, "b::<_, {link}>"),
                    Instr::Mcrr => write!(file, "mcrr"),
                    Instr::Mrrc => write!(file, "mrrc"),
                    Instr::Stc => write!(file, "stc"),
                    Instr::Ldc => write!(file, "ldc"),
                    Instr::Cdp => write!(file, "cdp"),
                    Instr::Mcr => write!(file, "mcr"),
                    Instr::Mrc => write!(file, "mrc"),
                    Instr::Swi => write!(file, "swi"),
                    Instr::Undefined { .. } => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_arm_uncond_instr_table(
        filename: &str,
        table: &[arm::UncondInstr],
    ) -> Result<(), io::Error> {
        use arm::UncondInstr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    UncondInstr::Pld => write!(file, "pld"),
                    UncondInstr::BlxImm => write!(file, "blx_imm"),
                    UncondInstr::Mcrr => write!(file, "mcrr"),
                    UncondInstr::Mrrc => write!(file, "mrrc"),
                    UncondInstr::Stc => write!(file, "stc"),
                    UncondInstr::Ldc => write!(file, "ldc"),
                    UncondInstr::Cdp => write!(file, "cdp"),
                    UncondInstr::Mcr => write!(file, "mcr"),
                    UncondInstr::Mrc => write!(file, "mrc"),
                    UncondInstr::Undefined => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_thumb_instr_table(filename: &str, table: &[thumb::Instr]) -> Result<(), io::Error> {
        use thumb::Instr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::AddSubRegImm3 { sub, imm3, op } => {
                        write!(
                            file,
                            "add_sub_reg_imm3::<_, {}, {}, {}>",
                            sub,
                            imm3,
                            imm3 && op == 0
                        )
                    }
                    Instr::ShiftImm { ty, shift: _ } => {
                        write!(
                            file,
                            "shift_imm::<_, {{ShiftImmTy::{} as u8}}>",
                            ["Lsl", "Lsr", "Asr"][ty as usize]
                        )
                    }
                    Instr::DpOpImm8 { ty, reg: _ } => {
                        write!(
                            file,
                            "dp_op_imm8::<_, {{DpOpImm8Ty::{} as u8}}>",
                            ["Mov", "Cmp", "Add", "Sub"][ty as usize]
                        )
                    }
                    Instr::DpOpReg(ty) => {
                        write!(
                            file,
                            "dp_op_reg::<_, {{DpOpRegTy::{} as u8}}>",
                            [
                                "And", "Eor", "Lsl", "Lsr", "Asr", "Adc", "Sbc", "Ror", "Tst",
                                "Neg", "Cmp", "Cmn", "Orr", "Mul", "Bic", "Mvn",
                            ][ty as usize],
                        )
                    }
                    Instr::Bx {
                        link,
                        addr_high_reg: _,
                    } => write!(file, "bx::<_, {link}>"),
                    Instr::DpOpSpecial { ty, h1: _, h2: _ } => {
                        write!(file, "{}_special", ["add", "cmp", "mov"][ty as usize])
                    }
                    Instr::LoadPcRel { reg: _ } => {
                        write!(file, "ldr_pc_rel")
                    }
                    Instr::LoadStoreReg { ty, offset_reg: _ } => {
                        let name = [
                            "str", "strh", "strb", "ldrsb", "ldr", "ldrh", "ldrb", "ldrsh",
                        ][ty as usize];
                        let has_imm_form = ty as u8 & 3 != 3;
                        write!(
                            file,
                            "{}{}",
                            name,
                            if has_imm_form { "::<_, false>" } else { "" }
                        )
                    }
                    Instr::LoadStoreWbImm {
                        byte,
                        load,
                        offset: _,
                    } => {
                        write!(
                            file,
                            "{}{}::<_, true>",
                            if load { "ldr" } else { "str" },
                            if byte { "b" } else { "" },
                        )
                    }
                    Instr::LoadStoreHalfImm { load, offset: _ } => {
                        write!(file, "{}h::<_, true>", if load { "ldr" } else { "str" })
                    }
                    Instr::LoadStoreStack { load, reg: _ } => {
                        write!(file, "{}_sp_rel", if load { "ldr" } else { "str" })
                    }
                    Instr::AddSpPcImm { sp, dst_reg: _ } => {
                        write!(file, "add_pc_sp_imm8::<_, {sp}>")
                    }
                    Instr::AddSubSpImm7 { sub } => {
                        write!(file, "add_sub_sp_imm7::<_, {sub}>")
                    }
                    Instr::PushPop {
                        pop,
                        push_r14_pop_r15,
                    } => {
                        write!(
                            file,
                            "{}::<_, {}>",
                            if pop { "pop" } else { "push" },
                            push_r14_pop_r15
                        )
                    }
                    Instr::Bkpt => {
                        write!(file, "bkpt")
                    }
                    Instr::LoadStoreMultiple { load, base_reg: _ } => {
                        write!(file, "{}ia", if load { "ldm" } else { "stm" })
                    }
                    Instr::Swi => write!(file, "swi"),
                    Instr::CondBranch { cond } => write!(file, "b_cond::<_, {cond}>"),
                    Instr::Branch => write!(file, "b"),
                    Instr::BlPrefix => write!(file, "bl_prefix"),
                    Instr::BlSuffix { exchange } => write!(file, "bl_suffix::<_, {exchange}>"),
                    Instr::Undefined { .. } => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    pub fn output_instr_tables() {
        output_arm_cond_instr_table("jit_arm7_arm.rs", &arm::cond(Processor::Arm7Tdmi), false)
            .expect("couldn't write JIT ARM7 ARM instruction table");
        output_thumb_instr_table(
            "jit_arm7_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm7Tdmi),
        )
        .expect("couldn't write JIT ARM7 thumb instruction table");

        output_arm_cond_instr_table("jit_arm9_arm_cond.rs", &arm::cond(Processor::Arm9Es), true)
            .expect("couldn't write JIT ARM9 ARM cond instruction table");
        output_arm_uncond_instr_table("jit_arm9_arm_uncond.rs", &arm::uncond(Processor::Arm9Es))
            .expect("couldn't write JIT ARM9 ARM uncond instruction table");
        output_thumb_instr_table(
            "jit_arm9_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm9Es),
        )
        .expect("couldn't write JIT ARM9 thumb instruction table");
    }
}

#[cfg(feature = "disasm")]
mod disasm {
    use super::*;

    fn output_arm_cond_instr_table(
        filename: &str,
        table: &[arm::Instr],
        is_arm9: bool,
    ) -> Result<(), io::Error> {
        use arm::{
            DpOperand, DspMulTy, Instr, MiscAddressing, MiscTransferTy, WbAddressing, WbOff,
        };
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::Mrs { spsr } => write!(file, "mrs::<{spsr}>"),
                    Instr::Msr { ty, spsr } => {
                        write!(file, "msr::<{}, {spsr}>", ty != arm::MsrTy::Reg)
                    }
                    Instr::Bx { link } => {
                        write!(file, "bx::<{link}>")
                    }
                    Instr::Clz => write!(file, "clz"),
                    Instr::SatAddSub { sub, doubled } => {
                        write!(file, "qaddsub::<{sub}, {doubled}>")
                    }
                    Instr::Bkpt => write!(file, "bkpt"),
                    Instr::DspMul(ty) => {
                        if is_arm9 {
                            match ty {
                                DspMulTy::Smulxy { acc } => write!(file, "smulxy::<{acc}>"),
                                DspMulTy::Smulwy { acc } => write!(file, "smulwy::<{acc}>"),
                                DspMulTy::Smlalxy => write!(file, "smlalxy"),
                            }
                        } else {
                            write!(file, "invalid_dsp_mul")
                        }
                    }
                    Instr::DpOp { ty, set_flags, op } => {
                        write!(
                            file,
                            "dp_op::<{{DpOpTy::{} as u8}}, ",
                            [
                                "And", "Eor", "Sub", "Rsb", "Add", "Adc", "Sbc", "Rsc", "Tst",
                                "Teq", "Cmp", "Cmn", "Orr", "Mov", "Bic", "Mvn",
                            ][ty as usize]
                        )?;
                        match op {
                            DpOperand::Imm => write!(file, "{{DpOperand::IMM}}"),
                            DpOperand::Reg {
                                shift_ty,
                                shift_imm,
                            } => write!(
                                file,
                                "{{DpOperand::encode_reg({}, {})}}",
                                arm_shift_ty_to_str(shift_ty),
                                shift_imm
                            ),
                        }?;
                        write!(file, ", {set_flags}>")
                    }
                    Instr::Mul { acc, set_flags } => {
                        write!(file, "mul::<{acc}, {set_flags}>")
                    }
                    Instr::MulLong {
                        acc,
                        set_flags,
                        signed,
                    } => write!(file, "umull_smull::<{signed}, {acc}, {set_flags}>"),
                    Instr::Swp { byte } => write!(file, "swp_swpb::<{byte}>"),
                    Instr::MiscTransfer {
                        ty,
                        addressing,
                        offset_upwards,
                        offset_imm,
                    } => {
                        write!(
                            file,
                            "load_store_misc::<{}, {{{}}}, {}, {}, {{MiscAddressing::{} as u8}}>",
                            !matches!(ty, MiscTransferTy::Strh | MiscTransferTy::Strd),
                            match ty {
                                MiscTransferTy::Ldrh => "LoadStoreMiscTy::Half as u8",
                                MiscTransferTy::Strh => "LoadStoreMiscTy::Half as u8",
                                MiscTransferTy::Ldrd => "LoadStoreMiscTy::Double as u8",
                                MiscTransferTy::Strd => "LoadStoreMiscTy::Double as u8",
                                MiscTransferTy::Ldrsb => "LoadStoreMiscTy::SignedByte as u8",
                                MiscTransferTy::Ldrsh => "LoadStoreMiscTy::SignedHalf as u8",
                            },
                            offset_imm,
                            offset_upwards,
                            match addressing {
                                MiscAddressing::Post => "Post",
                                MiscAddressing::PreNoWb => "PreNoWb",
                                MiscAddressing::Pre => "Pre",
                            }
                        )
                    }
                    Instr::WbTransfer {
                        load,
                        byte,
                        addressing,
                        offset_upwards,
                        offset,
                    } => {
                        let addressing = match addressing {
                            WbAddressing::Post => "Post",
                            WbAddressing::PostUser => "PostUser",
                            WbAddressing::PreNoWb => "PreNoWb",
                            WbAddressing::Pre => "Pre",
                        };
                        match offset {
                            WbOff::Reg(shift_ty) => write!(
                                file,
                                "load_store_wb::<{}, {}, {{WbOffTy::encode_reg({})}}, {}, \
                                 {{WbAddressing::{} as u8}}>",
                                load,
                                byte,
                                arm_shift_ty_to_str(shift_ty),
                                offset_upwards,
                                addressing
                            ),
                            WbOff::Imm => write!(
                                file,
                                "load_store_wb::<{}, {}, {{WbOffTy::IMM}}, {}, \
                                 {{WbAddressing::{} as u8}}>",
                                load, byte, offset_upwards, addressing
                            ),
                        }
                    }
                    Instr::TransferMultiple {
                        load,
                        increment,
                        base_excluded,
                        writeback,
                        s_bit,
                    } => {
                        write!(
                            file,
                            "ldm_stm::<{}, {}, {}, {}, {}>",
                            load, increment, base_excluded, writeback, s_bit
                        )
                    }
                    Instr::Branch { link } => write!(file, "b::<{link}>"),
                    Instr::Mcrr => write!(file, "mrrc_mcrr::<false>"),
                    Instr::Mrrc => write!(file, "mrrc_mcrr::<true>"),
                    Instr::Stc => write!(file, "ldc_stc::<false>"),
                    Instr::Ldc => write!(file, "ldc_stc::<true>"),
                    Instr::Cdp => write!(file, "cdp"),
                    Instr::Mcr => write!(file, "mrc_mcr::<false>"),
                    Instr::Mrc => write!(file, "mrc_mcr::<true>"),
                    Instr::Swi => write!(file, "swi"),
                    Instr::Undefined { .. } => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_arm_uncond_instr_table(
        filename: &str,
        table: &[arm::UncondInstr],
    ) -> Result<(), io::Error> {
        use arm::UncondInstr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    UncondInstr::Pld => write!(file, "pld"),
                    UncondInstr::BlxImm => write!(file, "blx_imm"),
                    UncondInstr::Stc => write!(file, "ldc2_stc2::<false>"),
                    UncondInstr::Ldc => write!(file, "ldc2_stc2::<true>"),
                    UncondInstr::Cdp => write!(file, "cdp2"),
                    UncondInstr::Mcr => write!(file, "mrc2_mcr2::<false>"),
                    UncondInstr::Mrc => write!(file, "mrc2_mcr2::<true>"),
                    UncondInstr::Undefined => write!(file, "undefined_uncond"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    fn output_thumb_instr_table(filename: &str, table: &[thumb::Instr]) -> Result<(), io::Error> {
        use thumb::Instr;
        let mut file = BufWriter::new(File::create(format!(
            "{}/{}",
            env::var("OUT_DIR").unwrap(),
            filename
        ))?);
        writeln!(file, "[")?;
        for chunk in table.as_chunks::<8>().0 {
            for instr in chunk.iter() {
                match *instr {
                    Instr::AddSubRegImm3 { sub, imm3, .. } => {
                        write!(file, "add_sub_reg_imm3::<{sub}, {imm3}>")
                    }
                    Instr::ShiftImm { ty, shift: _ } => {
                        write!(
                            file,
                            "shift_imm::<{{ShiftImmTy::{} as u8}}>",
                            ["Lsl", "Lsr", "Asr"][ty as usize]
                        )
                    }
                    Instr::DpOpImm8 { ty, reg: _ } => {
                        write!(
                            file,
                            "dp_op_imm8::<{{DpOpImm8Ty::{} as u8}}>",
                            ["Mov", "Cmp", "Add", "Sub"][ty as usize]
                        )
                    }
                    Instr::DpOpReg(ty) => {
                        write!(
                            file,
                            "dp_op_reg::<{{DpOpRegTy::{} as u8}}>",
                            [
                                "And", "Eor", "Lsl", "Lsr", "Asr", "Adc", "Sbc", "Ror", "Tst",
                                "Neg", "Cmp", "Cmn", "Orr", "Mul", "Bic", "Mvn",
                            ][ty as usize],
                        )
                    }
                    Instr::Bx {
                        link,
                        addr_high_reg: _,
                    } => {
                        write!(file, "bx::<{link}>")
                    }
                    Instr::DpOpSpecial { ty, h1: _, h2: _ } => {
                        write!(
                            file,
                            "dp_op_special::<{{DpOpSpecialTy::{} as u8}}>",
                            ["Add", "Cmp", "Mov"][ty as usize]
                        )
                    }
                    Instr::LoadPcRel { reg: _ } => {
                        write!(file, "ldr_pc_rel")
                    }
                    Instr::LoadStoreReg { ty, offset_reg: _ } => {
                        let ty = [
                            "Str", "Strh", "Strb", "Ldrsb", "Ldr", "Ldrh", "Ldrb", "Ldrsh",
                        ][ty as usize];
                        write!(
                            file,
                            "ldr_str::<{{ThumbLoadStoreTy::{ty} as u8}}, 0, false>"
                        )
                    }
                    Instr::LoadStoreWbImm {
                        byte,
                        load,
                        offset: _,
                    } => {
                        write!(
                            file,
                            "ldr_str::<{{ThumbLoadStoreTy::{}{} as u8}}, {}, true>",
                            if load { "Ldr" } else { "Str" },
                            if byte { "b" } else { "" },
                            if byte { 0 } else { 2 },
                        )
                    }
                    Instr::LoadStoreHalfImm { load, offset: _ } => {
                        write!(
                            file,
                            "ldr_str::<{{ThumbLoadStoreTy::{}h as u8}}, 1, true>",
                            if load { "Ldr" } else { "Str" }
                        )
                    }
                    Instr::LoadStoreStack { load, reg: _ } => {
                        write!(file, "ldr_str_sp_rel::<{load}>")
                    }
                    Instr::AddSpPcImm { sp, dst_reg: _ } => {
                        write!(file, "add_pc_sp_imm8::<{sp}>")
                    }
                    Instr::AddSubSpImm7 { sub } => {
                        write!(file, "add_sub_sp_imm7::<{sub}>")
                    }
                    Instr::PushPop { pop, .. } => {
                        write!(file, "push_pop::<{pop}>")
                    }
                    Instr::Bkpt => {
                        write!(file, "bkpt")
                    }
                    Instr::LoadStoreMultiple { load, base_reg: _ } => {
                        write!(file, "ldmia_stmia::<{load}>")
                    }
                    Instr::Swi => write!(file, "swi"),
                    Instr::CondBranch { cond } => write!(file, "b_cond::<{cond}>"),
                    Instr::Branch => write!(file, "b"),
                    Instr::BlPrefix => write!(file, "bl_prefix"),
                    Instr::BlSuffix { exchange } => {
                        write!(file, "bl_suffix::<{exchange}>")
                    }
                    Instr::Undefined { .. } => write!(file, "undefined"),
                    _ => unreachable!(),
                }?;
                write!(file, ",")?;
            }
            writeln!(file)?;
        }
        writeln!(file, "]")
    }

    pub fn output_instr_tables() {
        output_arm_cond_instr_table("disasm_arm7_arm.rs", &arm::cond(Processor::Arm7Tdmi), false)
            .expect("couldn't write disassembler ARM7 ARM instruction table");
        output_thumb_instr_table(
            "disasm_arm7_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm7Tdmi),
        )
        .expect("couldn't write disassembler ARM7 thumb instruction table");

        output_arm_cond_instr_table(
            "disasm_arm9_arm_cond.rs",
            &arm::cond(Processor::Arm9Es),
            true,
        )
        .expect("couldn't write disassembler ARM9 ARM cond instruction table");
        output_arm_uncond_instr_table("disasm_arm9_arm_uncond.rs", &arm::uncond(Processor::Arm9Es))
            .expect("couldn't write disassembler ARM9 ARM uncond instruction table");
        output_thumb_instr_table(
            "disasm_arm9_thumb.rs",
            &thumb::thumb(arm_decoder::Processor::Arm9Es),
        )
        .expect("couldn't write disassembler ARM9 thumb instruction table");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Tells dependents' build scripts (as `DEP_DUST_CORE_LOG` and `DEP_DUST_CORE_DEBUGGER_HOOKS`)
    // whether the constructors take loggers and `RunOutput` has the debugger hook variants, since
    // another crate in the same build can enable those features.
    if env::var_os("CARGO_FEATURE_LOG").is_some() {
        println!("cargo:log=1");
    }
    if env::var_os("CARGO_FEATURE_DEBUGGER_HOOKS").is_some() {
        println!("cargo:debugger_hooks=1");
    }
    interpreter::output_instr_tables();
    #[cfg(feature = "jit")]
    jit::output_instr_tables();
    #[cfg(feature = "disasm")]
    disasm::output_instr_tables();
}
