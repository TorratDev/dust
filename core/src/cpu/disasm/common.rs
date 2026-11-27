pub use super::super::engines_common::*;

pub static COND_STRINGS: [&str; 16] = [
    "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "", "nv",
];

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DpOpSpecialTy {
    Add,
    Cmp,
    Mov,
}

impl DpOpSpecialTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Add,
            1 => Self::Cmp,
            2 => Self::Mov,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadStoreMiscTy {
    Half,
    Double,
    SignedByte,
    SignedHalf,
}

impl LoadStoreMiscTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Half,
            1 => Self::Double,
            2 => Self::SignedByte,
            3 => Self::SignedHalf,
            _ => unreachable!(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThumbLoadStoreTy {
    Str,
    Strh,
    Strb,
    Ldrsb,
    Ldr,
    Ldrh,
    Ldrb,
    Ldrsh,
}

impl ThumbLoadStoreTy {
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Str,
            1 => Self::Strh,
            2 => Self::Strb,
            3 => Self::Ldrsb,
            4 => Self::Ldr,
            5 => Self::Ldrh,
            6 => Self::Ldrb,
            7 => Self::Ldrsh,
            _ => unreachable!(),
        }
    }
}
