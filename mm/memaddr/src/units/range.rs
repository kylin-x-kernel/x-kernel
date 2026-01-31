// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{fmt, ops::Range};

use crate::{MemoryAddr, PhysAddr, VirtAddr};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AddrRange<A: MemoryAddr> {
    pub start: A,
    pub end: A,
}

impl<A> AddrRange<A>
where
    A: MemoryAddr,
{
    #[inline]
    pub fn new(start: A, end: A) -> Self {
        if start <= end {
            Self { start, end }
        } else {
            panic!("invalid `AddrRange`: {}..{}", start.into(), end.into());
        }
    }

    #[inline]
    pub fn try_new(start: A, end: A) -> Option<Self> {
        if start <= end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    #[inline]
    /// Creates a range without checking that `start <= end`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `start <= end`. Violating this may break
    /// invariants expected by users of `AddrRange`.
    pub const unsafe fn new_unchecked(start: A, end: A) -> Self {
        Self { start, end }
    }

    #[inline]
    pub fn from_start_size(start: A, size: usize) -> Self {
        match start.checked_add(size) {
            Some(end) => Self { start, end },
            None => panic!(
                "size too large for `AddrRange`: {} + {}",
                start.into(),
                size
            ),
        }
    }

    #[inline]
    pub fn try_from_start_size(start: A, size: usize) -> Option<Self> {
        start.checked_add(size).map(|end| Self { start, end })
    }

    #[inline]
    /// Creates a range from `start` and `size` without overflow checks.
    ///
    /// # Safety
    ///
    /// The caller must ensure `start + size` does not overflow and that the
    /// resulting end is valid for the address type.
    pub unsafe fn from_start_size_unchecked(start: A, size: usize) -> Self {
        let end = start.wrapping_add(size);
        Self { start, end }
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    #[inline]
    pub fn size(self) -> usize {
        self.end.wrapping_sub_addr(self.start)
    }

    #[inline]
    pub fn contains(self, addr: A) -> bool {
        self.start <= addr && addr < self.end
    }

    #[inline]
    pub fn contains_range(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    #[inline]
    pub fn contained_in(self, other: Self) -> bool {
        other.contains_range(self)
    }

    #[inline]
    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

impl<A, T> TryFrom<Range<T>> for AddrRange<A>
where
    A: MemoryAddr + From<T>,
{
    type Error = ();

    #[inline]
    fn try_from(range: Range<T>) -> Result<Self, Self::Error> {
        Self::try_new(range.start.into(), range.end.into()).ok_or(())
    }
}

impl<A> Default for AddrRange<A>
where
    A: MemoryAddr,
{
    #[inline]
    fn default() -> Self {
        Self {
            start: 0.into(),
            end: 0.into(),
        }
    }
}

impl<A> fmt::Debug for AddrRange<A>
where
    A: MemoryAddr + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}..{:?}", self.start, self.end)
    }
}

impl<A> fmt::LowerHex for AddrRange<A>
where
    A: MemoryAddr + fmt::LowerHex,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:x}..{:x}", self.start, self.end)
    }
}

impl<A> fmt::UpperHex for AddrRange<A>
where
    A: MemoryAddr + fmt::UpperHex,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:X}..{:X}", self.start, self.end)
    }
}

pub type VirtAddrRange = AddrRange<VirtAddr>;
pub type PhysAddrRange = AddrRange<PhysAddr>;

#[macro_export]
macro_rules! addr_range {
    ($range:expr) => {
        $crate::AddrRange::try_from($range).expect("invalid address range in `addr_range!`")
    };
}

#[macro_export]
macro_rules! va_range {
    ($range:expr) => {
        $crate::VirtAddrRange::try_from($range).expect("invalid address range in `va_range!`")
    };
}

#[macro_export]
macro_rules! pa_range {
    ($range:expr) => {
        $crate::PhysAddrRange::try_from($range).expect("invalid address range in `pa_range!`")
    };
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_range {
    use unittest::def_test;

    use super::AddrRange;
    use crate::VirtAddr;

    #[def_test]
    fn test_range_size_and_empty() {
        let range = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
        assert_eq!(range.size(), 0x1000);
        assert!(!range.is_empty());
        let empty = AddrRange::new(VirtAddr::from(0x1000usize), VirtAddr::from(0x1000usize));
        assert!(empty.is_empty());
    }

    #[def_test]
    fn test_range_contains_range() {
        let outer = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x3000);
        let inner = AddrRange::from_start_size(VirtAddr::from(0x2000usize), 0x1000);
        assert!(outer.contains_range(inner));
        assert!(inner.contained_in(outer));
    }

    #[def_test]
    fn test_range_overlap() {
        let a = AddrRange::from_start_size(VirtAddr::from(0x1000usize), 0x1000);
        let b = AddrRange::from_start_size(VirtAddr::from(0x1800usize), 0x1000);
        let c = AddrRange::from_start_size(VirtAddr::from(0x3000usize), 0x1000);
        assert!(a.overlaps(b));
        assert!(!a.overlaps(c));
    }
}
