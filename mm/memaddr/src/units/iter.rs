// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::MemoryAddr;

pub struct PageIter<const PAGE_SIZE: usize, A>
where
    A: MemoryAddr,
{
    next: A,
    limit: A,
}

impl<A, const PAGE_SIZE: usize> PageIter<PAGE_SIZE, A>
where
    A: MemoryAddr,
{
    pub fn new(start: A, end: A) -> Option<Self> {
        Self::with_bounds(start, end)
    }

    pub fn with_bounds(start: A, end: A) -> Option<Self> {
        if PAGE_SIZE.is_power_of_two() && start.is_aligned(PAGE_SIZE) && end.is_aligned(PAGE_SIZE) {
            Some(Self {
                next: start,
                limit: end,
            })
        } else {
            None
        }
    }
}

impl<A, const PAGE_SIZE: usize> Iterator for PageIter<PAGE_SIZE, A>
where
    A: MemoryAddr,
{
    type Item = A;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next < self.limit {
            let out = self.next;
            self.next = self.next.add(PAGE_SIZE);
            Some(out)
        } else {
            None
        }
    }
}

pub struct DynPageIter<A>
where
    A: MemoryAddr,
{
    cursor: A,
    limit: A,
    step: usize,
}

impl<A> DynPageIter<A>
where
    A: MemoryAddr,
{
    pub fn new(start: A, end: A, page_size: usize) -> Option<Self> {
        Self::with_bounds(start, end, page_size)
    }

    pub fn with_bounds(start: A, end: A, page_size: usize) -> Option<Self> {
        if page_size.is_power_of_two() && start.is_aligned(page_size) && end.is_aligned(page_size) {
            Some(Self {
                cursor: start,
                limit: end,
                step: page_size,
            })
        } else {
            None
        }
    }
}

impl<A> Iterator for DynPageIter<A>
where
    A: MemoryAddr,
{
    type Item = A;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor < self.limit {
            let out = self.cursor;
            self.cursor = self.cursor.add(self.step);
            Some(out)
        } else {
            None
        }
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_iter {
    use unittest::def_test;

    use super::{DynPageIter, PageIter};
    use crate::{PAGE_SIZE_4K, VirtAddr};

    #[def_test]
    fn test_page_iter_steps() {
        let start = VirtAddr::from(0x1000usize);
        let end = VirtAddr::from(0x3000usize);
        let mut iter = PageIter::<PAGE_SIZE_4K, _>::new(start, end).unwrap();
        assert_eq!(iter.next().unwrap(), start);
        assert_eq!(iter.next().unwrap(), VirtAddr::from(0x2000usize));
        assert!(iter.next().is_none());
    }

    #[def_test]
    fn test_dyn_page_iter_steps() {
        let start = VirtAddr::from(0x0usize);
        let end = VirtAddr::from(0x2000usize);
        let mut iter = DynPageIter::new(start, end, PAGE_SIZE_4K).unwrap();
        assert_eq!(iter.next().unwrap(), start);
        assert_eq!(iter.next().unwrap(), VirtAddr::from(0x1000usize));
        assert!(iter.next().is_none());
    }

    #[def_test]
    fn test_page_iter_invalid_alignment() {
        let start = VirtAddr::from(0x1001usize);
        let end = VirtAddr::from(0x2000usize);
        assert!(PageIter::<PAGE_SIZE_4K, _>::new(start, end).is_none());
    }
}
