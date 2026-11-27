use std::{iter, ops};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RangeInclusive<Idx> {
    pub start: Idx,
    pub end: Idx,
}

impl<Idx> RangeInclusive<Idx> {
    #[inline]
    pub fn contains<U>(&self, item: &U) -> bool
    where
        U: ?Sized + PartialOrd<Idx>,
        Idx: PartialOrd<U>,
    {
        *item >= self.start && *item <= self.end
    }
}

impl<Idx> RangeInclusive<Idx>
where
    Idx: PartialOrd,
{
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }
}

impl<Idx> From<(Idx, Idx)> for RangeInclusive<Idx> {
    #[inline]
    fn from((start, end): (Idx, Idx)) -> Self {
        RangeInclusive { start, end }
    }
}

impl<Idx> From<RangeInclusive<Idx>> for (Idx, Idx) {
    #[inline]
    fn from(range: RangeInclusive<Idx>) -> Self {
        (range.start, range.end)
    }
}

pub trait RangeIndex: Copy {
    fn into_u64(self) -> u64;
    fn from_u64(value: u64) -> Self;
}

impl RangeIndex for u32 {
    fn into_u64(self) -> u64 {
        u64::from(self)
    }

    fn from_u64(value: u64) -> Self {
        value as u32
    }
}

impl RangeIndex for u64 {
    fn into_u64(self) -> u64 {
        self
    }

    fn from_u64(value: u64) -> Self {
        value
    }
}

impl<Idx: RangeIndex> IntoIterator for RangeInclusive<Idx> {
    type IntoIter = iter::Map<ops::RangeInclusive<u64>, fn(u64) -> Idx>;
    type Item = Idx;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        (self.start.into_u64()..=self.end.into_u64()).map(Idx::from_u64)
    }
}
