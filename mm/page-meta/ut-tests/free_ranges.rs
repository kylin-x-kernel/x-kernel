// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{vec, vec::Vec};
use core::ptr::NonNull;
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddrRange};
use unittest::def_test;

use super::{normalize, with_reserved_storage};
use crate::PageMetaError;

struct Storage {
    bytes: Vec<u8>,
    start: usize,
}

impl Storage {
    fn new(pages: usize) -> Self {
        let bytes = vec![0xa5; (pages + 1) * PAGE];
        let start = (bytes.as_ptr() as usize + PAGE - 1) & !(PAGE - 1);
        Self { bytes, start }
    }

    fn map(&self, backing: PhysAddrRange, first_pfn: usize) -> Result<NonNull<u8>, PageMetaError> {
        let offset = backing.start.as_usize().checked_sub(first_pfn * PAGE).ok_or(PageMetaError::InvalidRange)?;
        assert!(self.start + offset + backing.size() <= self.bytes.as_ptr() as usize + self.bytes.len());
        NonNull::new((self.start + offset) as *mut u8).ok_or(PageMetaError::InvalidRange)
    }
}

fn total_pages(ranges: &[PhysAddrRange]) -> usize {
    ranges.iter().map(|r| r.size() / PAGE).sum()
}

fn range(first_pfn: usize, pages: usize) -> PhysAddrRange {
    PhysAddrRange::from_start_size((first_pfn * PAGE).into(), pages * PAGE)
}

#[def_test]
fn init_maps_only_backing_and_preserves_full_physical_coverage() {
    let first = Storage::new(8);
    let second = Storage::new(4);
    let mut free = [range(512, 4), range(256, 8)];
    let input = free;
    let mut mapped = Vec::with_capacity(1);
    // SAFETY: the fixture exclusively owns both modeled RAM ranges. The
    // callback maps the complete selected backing into disjoint live storage,
    // retained until the table and all descriptor borrows are destroyed.
    let table = unsafe {
        crate::init(&mut free, |backing| {
            mapped.push(backing);
            if input[0].contains_range(backing) {
                second.map(backing, 512)
            } else {
                assert!(input[1].contains_range(backing));
                first.map(backing, 256)
            }
        })
    }.unwrap();
    assert_eq!(table.covered_pages(), 12);
    let storage_pages = mapped.iter().map(|r| r.size() / PAGE).sum::<usize>();
    assert_eq!(table.covered_pages(), storage_pages + total_pages(&free));
    for original in input {
        for pfn in original.start.as_usize() / PAGE..original.end.as_usize() / PAGE {
            let meta = table.lookup(pfn).unwrap();
            assert!(core::ptr::eq(meta, table.lookup(pfn).unwrap()));
            let is_free = free.iter().any(|r| r.contains((pfn * PAGE).into()));
            assert_eq!(meta.is_permanent(), !is_free);
            assert_eq!(meta.is_permanent(), mapped.iter().any(|r| r.contains((pfn * PAGE).into())));
        }
    }
    for pfn in [0, 255, 264, 511, 516, usize::MAX] {
        assert!(table.lookup(pfn).is_none());
    }
    assert_eq!(free[0].start, input[1].start);
    assert_eq!(free[1].start, input[0].start);
}

#[def_test]
fn adjacent_physical_coverage_does_not_require_a_shared_content_mapping() {
    let first = Storage::new(4);
    let second = Storage::new(4);
    let mut free = [range(256, 4), range(260, 4)];
    let mut mapped = 0;
    // SAFETY: the two allocations provide exclusive writable mappings for the
    // modeled RAM. Only selected backing is mapped, and both fixtures outlive
    // every lookup borrow and the initialized table.
    let table = unsafe {
        crate::init(&mut free, |backing| {
            mapped += 1;
            if range(256, 4).contains_range(backing) {
                first.map(backing, 256)
            } else {
                assert!(range(260, 4).contains_range(backing));
                second.map(backing, 260)
            }
        })
    }.unwrap();
    assert_eq!(mapped, 1);
    assert_eq!(table.covered_pages(), 8);
    assert!((256..264).all(|pfn| table.lookup(pfn).is_some()));
    assert!(free[1].is_empty());
}

#[def_test]
fn caller_ranges_are_normalized_and_support_multiple_storage_requests() {
    let mut free = [
        PhysAddrRange::from_start_size((PAGE + 7).into(), 6 * PAGE),
        range(16, 4),
    ];
    assert_eq!(normalize(&mut free), Ok(()));
    assert_eq!(total_pages(&free), 9);
    assert_eq!(free[0], range(2, 5));
    let a = with_reserved_storage(&mut free, PAGE + 1, Ok).unwrap();
    let b = with_reserved_storage(&mut free, 3 * PAGE, Ok).unwrap();
    assert_eq!(a.size(), 2 * PAGE);
    assert_eq!(b.size(), 3 * PAGE);
    assert_eq!(total_pages(&free), 4);
    for r in free {
        for backing in [a, b] {
            assert!(r.end <= backing.start || backing.end <= r.start);
        }
    }
}

#[def_test]
fn invalid_ranges_do_not_map_or_write_storage() {
    let storage = Storage::new(4);
    let mut free = [range(256, 4), range(256, 4)];
    let mut mapped = false;
    // SAFETY: the live fixture is exclusive. The overlapping modeled input is
    // rejected before the callback can map or initialize metadata storage.
    let result = unsafe { crate::init(&mut free, |_| { mapped = true; Err(PageMetaError::InvalidRange) }) };
    assert!(matches!(result, Err(PageMetaError::Overlap)));
    assert!(!mapped);
    assert!(storage.bytes.iter().all(|&byte| byte == 0xa5));
    assert_eq!(normalize(&mut []), Err(PageMetaError::EmptyCoverage));
    let mut reversed = [PhysAddrRange { start: (2 * PAGE).into(), end: PAGE.into() }];
    assert_eq!(normalize(&mut reversed), Err(PageMetaError::InvalidRange));
    let mut unalignable = [PhysAddrRange { start: (usize::MAX - 1).into(), end: usize::MAX.into() }];
    assert_eq!(normalize(&mut unalignable), Err(PageMetaError::InvalidRange));
}

#[def_test]
fn failed_mapping_or_reservation_does_not_consume_free_ram() {
    let storage = Storage::new(4);
    let mut free = [range(256, 4)];
    let original = free;
    // SAFETY: the modeled RAM is exclusively retained by the live fixture.
    // The failed callback supplies no pointer and performs no memory access.
    let result = unsafe { crate::init(&mut free, |_| Err(PageMetaError::NoMemory)) };
    assert!(matches!(result, Err(PageMetaError::NoMemory)));
    assert_eq!(free, original);
    assert_eq!(with_reserved_storage(&mut free, 5 * PAGE, Ok), Err(PageMetaError::NoMemory));
    assert_eq!(with_reserved_storage(&mut free, usize::MAX, Ok), Err(PageMetaError::InvalidRange));
    assert_eq!(free, original);
    assert!(storage.bytes.iter().all(|&byte| byte == 0xa5));
}

#[def_test]
fn final_storage_reservation_can_leave_an_empty_entry() {
    let storage = Storage::new(1);
    let mut free = [range(256, 1)];
    // SAFETY: the full one-page fixture is exclusive writable backing and
    // remains live until the table and all metadata borrows are destroyed.
    let table = unsafe { crate::init(&mut free, |backing| storage.map(backing, 256)) }.unwrap();
    assert_eq!(table.covered_pages(), 1);
    assert_eq!(table.covered_pages() - total_pages(&free), 1);
    assert!(table.lookup(256).unwrap().is_permanent());
    assert!(free[0].is_empty());
}

#[def_test]
fn input_length_is_independent_of_backend_index_capacity() {
    let storage = Storage::new(129);
    let mut free: Vec<_> = (0..129).map(|i| range(256 + i, 1)).collect();
    let mut storage_pages = 0;
    // SAFETY: the fixture exclusively supplies all modeled contiguous RAM;
    // the callback maps selected backing within the retained live allocation.
    let table = unsafe {
        crate::init(&mut free, |backing| {
            storage_pages += backing.size() / PAGE;
            storage.map(backing, 256)
        })
    }.unwrap();
    assert_eq!(table.covered_pages(), 129);
    assert_eq!(table.covered_pages(), storage_pages + total_pages(&free));
    assert_eq!(free.iter().filter(|r| !r.is_empty()).count(), 1);
}
