// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual memory set management and mapping utilities.
#![cfg_attr(not(test), no_std)]
extern crate alloc;

mod area;
mod backend;
mod set;

pub use self::{area::MemoryArea, backend::MemorySetBackend, set::MemorySet};

/// Error type for memory set operations.
#[derive(Debug, Eq, PartialEq)]
pub enum MemorySetError {
    /// Invalid parameter (e.g., `addr`, `size`, `flags`, etc.)
    InvalidParam,
    /// The given range overlaps with an existing mapping.
    AlreadyExists,
    /// The backend page table is in a bad state.
    BadState,
}

impl From<MemorySetError> for kerrno::KError {
    fn from(err: MemorySetError) -> Self {
        match err {
            MemorySetError::InvalidParam => kerrno::KError::InvalidInput,
            MemorySetError::AlreadyExists => kerrno::KError::AlreadyExists,
            MemorySetError::BadState => kerrno::KError::BadState,
        }
    }
}

/// A [`Result`] type with [`MemorySetError`] as the error type.
pub type MemorySetResult<T = ()> = Result<T, MemorySetError>;

#[cfg(unittest)]
mod tests_memset {
    use memaddr::{VirtAddr, va};
    use unittest::def_test;

    use super::{MemoryArea, MemorySet, MemorySetBackend};

    #[derive(Clone, Copy)]
    struct DummyBackend;

    impl MemorySetBackend for DummyBackend {
        type Addr = VirtAddr;
        type Flags = u8;
        type PageTable = ();

        fn map(
            &self,
            _start: Self::Addr,
            _size: usize,
            _flags: Self::Flags,
            _page_table: &mut Self::PageTable,
        ) -> bool {
            true
        }

        fn unmap(
            &self,
            _start: Self::Addr,
            _size: usize,
            _page_table: &mut Self::PageTable,
        ) -> bool {
            true
        }

        fn protect(
            &self,
            _start: Self::Addr,
            _size: usize,
            _new_flags: Self::Flags,
            _page_table: &mut Self::PageTable,
        ) -> bool {
            true
        }
    }

    #[def_test]
    fn test_memory_set_new_empty() {
        let set: MemorySet<DummyBackend> = MemorySet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
    }

    #[def_test]
    fn test_memory_set_map_and_find() {
        let mut set: MemorySet<DummyBackend> = MemorySet::new();
        let mut page_table = ();
        let area = MemoryArea::new(va!(0x1000), 0x1000, 0x1, DummyBackend);
        set.map(area, &mut page_table, false).unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.find(va!(0x1000)).is_some());
    }

    #[def_test]
    fn test_memory_set_overlaps() {
        let mut set: MemorySet<DummyBackend> = MemorySet::new();
        let mut page_table = ();
        let area = MemoryArea::new(va!(0x2000), 0x1000, 0x1, DummyBackend);
        set.map(area, &mut page_table, false).unwrap();
        let overlap = set.overlaps(memaddr::AddrRange::from_start_size(va!(0x2800), 0x100));
        assert!(overlap);
    }
}
