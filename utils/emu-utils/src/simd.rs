//! A small, stable subset of `core::simd` used by Dust.
//!
//! Operations are evaluated lane by lane. The public API intentionally mirrors the portable SIMD
//! operations used by the emulator so saved states and arithmetic retain their existing behavior.

#![allow(non_camel_case_types)]

use core::ops::{
    Add, AddAssign, BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Div, Index,
    IndexMut, Mul, MulAssign, Neg, Not, Rem, Shl, ShlAssign, Shr, ShrAssign, Sub, SubAssign,
};

pub trait SimdElement: Copy {
    type Mask: MaskElement;
}
macro_rules! impl_simd_element {
    ($($ty:ty => $mask:ty),* $(,)?) => {$(
        impl SimdElement for $ty {
            type Mask = $mask;
        }
    )*};
}
impl_simd_element!(
    u8 => i8, u16 => i16, u32 => i32, u64 => i64, usize => isize,
    i8 => i8, i16 => i16, i32 => i32, i64 => i64, isize => isize,
    f32 => i32, f64 => i64,
);

pub trait SimdCast<T>: SimdElement {
    fn cast_from(value: T) -> Self;
}
macro_rules! impl_simd_cast {
    ($src:ty => $($dst:ty),* $(,)?) => {$(
        impl SimdCast<$src> for $dst {
            #[inline]
            fn cast_from(value: $src) -> Self {
                value as $dst
            }
        }
    )*};
}
impl_simd_cast!(u8 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(u16 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(u32 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(u64 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(usize => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(i8 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(i16 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(i32 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(i64 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(isize => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(f32 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);
impl_simd_cast!(f64 => u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Simd<T, const LANES: usize>([T; LANES]);

impl<T: Default, const LANES: usize> Default for Simd<T, LANES> {
    fn default() -> Self {
        Self(core::array::from_fn(|_| T::default()))
    }
}

impl<T, const LANES: usize> Simd<T, LANES> {
    #[inline]
    pub const fn from_array(array: [T; LANES]) -> Self {
        Self(array)
    }

    #[inline]
    pub fn to_array(self) -> [T; LANES] {
        self.0
    }

    #[inline]
    pub const fn as_array(&self) -> &[T; LANES] {
        &self.0
    }

    #[inline]
    pub fn as_mut_array(&mut self) -> &mut [T; LANES] {
        &mut self.0
    }
}

impl<T: Copy, const LANES: usize> Simd<T, LANES> {
    #[inline]
    pub const fn splat(value: T) -> Self {
        Self([value; LANES])
    }

    #[inline]
    pub fn from_slice(slice: &[T]) -> Self {
        assert_eq!(slice.len(), LANES);
        Self(core::array::from_fn(|i| slice[i]))
    }
}

impl<T: SimdElement, const LANES: usize> Simd<T, LANES> {
    #[inline]
    pub fn cast<U>(self) -> Simd<U, LANES>
    where
        U: SimdCast<T>,
    {
        Simd(core::array::from_fn(|i| U::cast_from(self.0[i])))
    }

    #[inline]
    pub fn simd_eq(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialEq,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] == other.0[i])
        }))
    }

    #[inline]
    pub fn simd_ne(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialEq,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] != other.0[i])
        }))
    }

    #[inline]
    pub fn simd_lt(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialOrd,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] < other.0[i])
        }))
    }

    #[inline]
    pub fn simd_le(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialOrd,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] <= other.0[i])
        }))
    }

    #[inline]
    pub fn simd_gt(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialOrd,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] > other.0[i])
        }))
    }

    #[inline]
    pub fn simd_ge(self, other: Self) -> Mask<T::Mask, LANES>
    where
        T: PartialOrd,
    {
        Mask(core::array::from_fn(|i| {
            T::Mask::from_bool(self.0[i] >= other.0[i])
        }))
    }

    #[inline]
    pub fn simd_min(self, other: Self) -> Self
    where
        T: Ord,
    {
        Self(core::array::from_fn(|i| self.0[i].min(other.0[i])))
    }

    #[inline]
    pub fn simd_max(self, other: Self) -> Self
    where
        T: Ord,
    {
        Self(core::array::from_fn(|i| self.0[i].max(other.0[i])))
    }

    #[inline]
    pub fn reduce_sum(self) -> T
    where
        T: Default + Add<Output = T>,
    {
        self.0
            .into_iter()
            .fold(T::default(), |sum, value| sum + value)
    }

    #[inline]
    pub fn shr_lanes(self, rhs: Self) -> Self
    where
        T: Shr<Output = T>,
    {
        Self(core::array::from_fn(|i| self.0[i] >> rhs.0[i]))
    }
}

impl<T, const LANES: usize> Index<usize> for Simd<T, LANES> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl<T, const LANES: usize> IndexMut<usize> for Simd<T, LANES> {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

macro_rules! impl_binary_op {
    ($trait:ident, $method:ident) => {
        impl<T, const LANES: usize> $trait for Simd<T, LANES>
        where
            T: Copy + $trait<Output = T>,
        {
            type Output = Self;

            #[inline]
            fn $method(self, rhs: Self) -> Self::Output {
                Self(core::array::from_fn(|i| self.0[i].$method(rhs.0[i])))
            }
        }
    };
}
impl_binary_op!(Add, add);
impl_binary_op!(Sub, sub);
impl_binary_op!(Mul, mul);
impl_binary_op!(Div, div);
impl_binary_op!(Rem, rem);
impl_binary_op!(BitAnd, bitand);
impl_binary_op!(BitOr, bitor);
impl_binary_op!(BitXor, bitxor);

macro_rules! impl_assign_op {
    ($trait:ident, $method:ident) => {
        impl<T, const LANES: usize> $trait for Simd<T, LANES>
        where
            T: Copy + $trait,
        {
            #[inline]
            fn $method(&mut self, rhs: Self) {
                for i in 0..LANES {
                    self.0[i].$method(rhs.0[i]);
                }
            }
        }
    };
}
impl_assign_op!(AddAssign, add_assign);
impl_assign_op!(SubAssign, sub_assign);
impl_assign_op!(MulAssign, mul_assign);
impl_assign_op!(BitAndAssign, bitand_assign);
impl_assign_op!(BitOrAssign, bitor_assign);
impl_assign_op!(BitXorAssign, bitxor_assign);

macro_rules! impl_shift {
    ($trait:ident, $method:ident) => {
        impl<T, Rhs, const LANES: usize> $trait<Rhs> for Simd<T, LANES>
        where
            T: Copy + $trait<Rhs, Output = T>,
            Rhs: Copy,
        {
            type Output = Self;

            #[inline]
            fn $method(self, rhs: Rhs) -> Self::Output {
                Self(core::array::from_fn(|i| self.0[i].$method(rhs)))
            }
        }
    };
}
impl_shift!(Shl, shl);
impl_shift!(Shr, shr);

macro_rules! impl_shift_assign {
    ($trait:ident, $method:ident) => {
        impl<T, Rhs, const LANES: usize> $trait<Rhs> for Simd<T, LANES>
        where
            T: Copy + $trait<Rhs>,
            Rhs: Copy,
        {
            #[inline]
            fn $method(&mut self, rhs: Rhs) {
                for value in &mut self.0 {
                    value.$method(rhs);
                }
            }
        }
    };
}
impl_shift_assign!(ShlAssign, shl_assign);
impl_shift_assign!(ShrAssign, shr_assign);

impl<T, const LANES: usize> Not for Simd<T, LANES>
where
    T: Copy + Not<Output = T>,
{
    type Output = Self;

    #[inline]
    fn not(self) -> Self::Output {
        Self(core::array::from_fn(|i| !self.0[i]))
    }
}

impl<T, const LANES: usize> Neg for Simd<T, LANES>
where
    T: Copy + Neg<Output = T>,
{
    type Output = Self;

    #[inline]
    fn neg(self) -> Self::Output {
        Self(core::array::from_fn(|i| -self.0[i]))
    }
}

pub trait MaskElement: SimdElement {
    fn from_bool(value: bool) -> Self;
    fn is_true(self) -> bool;
}
macro_rules! impl_mask_element {
    ($($ty:ty),* $(,)?) => {$(
        impl MaskElement for $ty {
            #[inline]
            fn from_bool(value: bool) -> Self {
                if value { -1 } else { 0 }
            }

            #[inline]
            fn is_true(self) -> bool {
                self != 0
            }
        }
    )*};
}
impl_mask_element!(i8, i16, i32, i64, isize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Mask<T, const LANES: usize>([T; LANES]);

impl<T: Default, const LANES: usize> Default for Mask<T, LANES> {
    fn default() -> Self {
        Self(core::array::from_fn(|_| T::default()))
    }
}

impl<T: MaskElement, const LANES: usize> Mask<T, LANES> {
    #[inline]
    pub fn from_array(array: [bool; LANES]) -> Self {
        Self(core::array::from_fn(|i| T::from_bool(array[i])))
    }

    #[inline]
    pub fn any(self) -> bool {
        self.0.into_iter().any(T::is_true)
    }

    #[inline]
    pub fn all(self) -> bool {
        self.0.into_iter().all(T::is_true)
    }

    #[inline]
    pub fn to_int(self) -> Simd<T, LANES> {
        Simd(self.0)
    }

    #[inline]
    pub fn select<U: Copy>(
        self,
        if_true: Simd<U, LANES>,
        if_false: Simd<U, LANES>,
    ) -> Simd<U, LANES> {
        Simd(core::array::from_fn(|i| {
            if self.0[i].is_true() {
                if_true[i]
            } else {
                if_false[i]
            }
        }))
    }
}

macro_rules! impl_mask_binary_op {
    ($trait:ident, $method:ident) => {
        impl<T: MaskElement + $trait<Output = T>, const LANES: usize> $trait for Mask<T, LANES> {
            type Output = Self;

            #[inline]
            fn $method(self, rhs: Self) -> Self::Output {
                Self(core::array::from_fn(|i| self.0[i].$method(rhs.0[i])))
            }
        }
    };
}
impl_mask_binary_op!(BitAnd, bitand);
impl_mask_binary_op!(BitOr, bitor);
impl_mask_binary_op!(BitXor, bitxor);

impl<T: MaskElement + Not<Output = T>, const LANES: usize> Not for Mask<T, LANES> {
    type Output = Self;

    #[inline]
    fn not(self) -> Self::Output {
        Self(core::array::from_fn(|i| !self.0[i]))
    }
}

#[macro_export]
macro_rules! simd_swizzle {
    ($value:expr, [$($index:expr),* $(,)?]) => {{
        let value = $value;
        $crate::simd::Simd::from_array([$(value[$index]),*])
    }};
}
pub use crate::simd_swizzle;

pub type i16x2 = Simd<i16, 2>;
pub type i32x2 = Simd<i32, 2>;
pub type i32x4 = Simd<i32, 4>;
pub type i64x2 = Simd<i64, 2>;
pub type i64x4 = Simd<i64, 4>;
pub type u8x4 = Simd<u8, 4>;
pub type u16x2 = Simd<u16, 2>;
pub type u16x4 = Simd<u16, 4>;
pub type u32x2 = Simd<u32, 2>;
pub type u32x4 = Simd<u32, 4>;
pub type u32x8 = Simd<u32, 8>;
pub type u64x2 = Simd<u64, 2>;
pub type u64x4 = Simd<u64, 4>;
pub type f64x4 = Simd<f64, 4>;
pub type mask64x4 = Mask<i64, 4>;
