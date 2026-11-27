#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShiftTy {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

impl ShiftTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Lsl,
            1 => Self::Lsr,
            2 => Self::Asr,
            3 => Self::Ror,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShiftImmTy {
    Lsl,
    Lsr,
    Asr,
}

impl ShiftImmTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Lsl,
            1 => Self::Lsr,
            2 => Self::Asr,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpOpImm8Ty {
    Mov,
    Cmp,
    Add,
    Sub,
}

impl DpOpImm8Ty {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Mov,
            1 => Self::Cmp,
            2 => Self::Add,
            3 => Self::Sub,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpOpRegTy {
    And,
    Eor,
    Lsl,
    Lsr,
    Asr,
    Adc,
    Sbc,
    Ror,
    Tst,
    Neg,
    Cmp,
    Cmn,
    Orr,
    Mul,
    Bic,
    Mvn,
}

impl DpOpRegTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::And,
            1 => Self::Eor,
            2 => Self::Lsl,
            3 => Self::Lsr,
            4 => Self::Asr,
            5 => Self::Adc,
            6 => Self::Sbc,
            7 => Self::Ror,
            8 => Self::Tst,
            9 => Self::Neg,
            10 => Self::Cmp,
            11 => Self::Cmn,
            12 => Self::Orr,
            13 => Self::Mul,
            14 => Self::Bic,
            15 => Self::Mvn,
            _ => unreachable!(),
        }
    }

    pub const fn is_unary(self) -> bool {
        matches!(self, Self::Neg | Self::Mvn)
    }

    pub const fn is_shift(self) -> bool {
        matches!(self, Self::Lsl | Self::Lsr | Self::Asr | Self::Ror)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpOpTy {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
}

impl DpOpTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::And,
            1 => Self::Eor,
            2 => Self::Sub,
            3 => Self::Rsb,
            4 => Self::Add,
            5 => Self::Adc,
            6 => Self::Sbc,
            7 => Self::Rsc,
            8 => Self::Tst,
            9 => Self::Teq,
            10 => Self::Cmp,
            11 => Self::Cmn,
            12 => Self::Orr,
            13 => Self::Mov,
            14 => Self::Bic,
            15 => Self::Mvn,
            _ => unreachable!(),
        }
    }

    pub const fn is_test(self) -> bool {
        matches!(self, Self::Tst | Self::Teq | Self::Cmp | Self::Cmn)
    }

    pub const fn is_unary(self) -> bool {
        matches!(self, Self::Mov | Self::Mvn)
    }

    pub const fn sets_carry(self) -> bool {
        match self {
            Self::And
            | Self::Eor
            | Self::Tst
            | Self::Teq
            | Self::Orr
            | Self::Mov
            | Self::Bic
            | Self::Mvn => false,
            Self::Sub
            | Self::Rsb
            | Self::Add
            | Self::Adc
            | Self::Sbc
            | Self::Rsc
            | Self::Cmp
            | Self::Cmn => true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpOperand {
    Imm,
    Reg { shift_ty: ShiftTy, shift_imm: bool },
}

impl DpOperand {
    pub const IMM: u8 = 0;

    pub const fn encode_reg(shift_ty: ShiftTy, shift_imm: bool) -> u8 {
        1 | ((shift_ty as u8) << 1) | ((shift_imm as u8) << 3)
    }

    pub const fn from_u8(value: u8) -> Self {
        if value == Self::IMM {
            Self::Imm
        } else {
            Self::Reg {
                shift_ty: ShiftTy::from_u8((value >> 1) & 3),
                shift_imm: value & 8 != 0,
            }
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WbAddressing {
    Post,
    PostUser,
    PreNoWb,
    Pre,
}

impl WbAddressing {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Post,
            1 => Self::PostUser,
            2 => Self::PreNoWb,
            3 => Self::Pre,
            _ => unreachable!(),
        }
    }

    pub const fn preincrement(self) -> bool {
        matches!(self, WbAddressing::PreNoWb | WbAddressing::Pre)
    }

    pub const fn writeback(self) -> bool {
        !matches!(self, WbAddressing::PreNoWb)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WbOffTy {
    Imm,
    Reg(ShiftTy),
}

impl WbOffTy {
    pub const IMM: u8 = 0;

    pub const fn encode_reg(shift_ty: ShiftTy) -> u8 {
        shift_ty as u8 + 1
    }

    pub const fn from_u8(value: u8) -> Self {
        if value == Self::IMM {
            Self::Imm
        } else {
            Self::Reg(ShiftTy::from_u8(value - 1))
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MiscAddressing {
    Post,
    PreNoWb,
    Pre,
}

impl MiscAddressing {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Post,
            1 => Self::PreNoWb,
            2 => Self::Pre,
            _ => unreachable!(),
        }
    }

    pub const fn preincrement(self) -> bool {
        !matches!(self, MiscAddressing::Post)
    }

    pub const fn writeback(self) -> bool {
        !matches!(self, MiscAddressing::PreNoWb)
    }
}
