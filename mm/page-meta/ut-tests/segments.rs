// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Descriptor contracts using caller-owned storage without an allocator.

use core::{mem::size_of, ptr::NonNull};

use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddrRange};
use unittest::def_test;

use super::{CAPACITY, Segment, Table, build_segments, storage_bytes};
use crate::{PageMeta, PageMetaError, free_ranges};

#[repr(align(4096))]
struct Storage([u8; PAGE]);

impl Storage {
    fn new() -> Self {
        Self([0xa5; PAGE])
    }

    fn descriptors(&mut self) -> NonNull<PageMeta> {
        NonNull::from(&mut self.0).cast()
    }
}

fn range(first_pfn: usize, pages: usize) -> PhysAddrRange {
    PhysAddrRange::from_start_size((first_pfn * PAGE).into(), pages * PAGE)
}

/// Builds a local table over fixture-owned backing.
///
/// # Safety
/// Storage must remain exclusive and live until the table and all lookup
/// borrows end. The fixture models physical identities independently of mappings.
unsafe fn initialize(
    ranges: &mut [PhysAddrRange],
    storage: &mut Storage,
) -> Result<Table, PageMetaError> {
    free_ranges::normalize(ranges)?;
    let mut segments = [Segment::EMPTY; CAPACITY];
    let (segment_count, entry_count) = build_segments(ranges, &mut segments)?;
    // SAFETY: normalize and build_segments produced a matching valid index;
    // the caller retains exclusive aligned fixture storage for all borrows.
    unsafe {
        Table::initialize(
            segments,
            segment_count,
            entry_count,
            storage.descriptors(),
            PAGE,
        )
    }
}

#[def_test]
fn physical_directory_does_not_require_page_content_mappings() {
    let mut storage = Storage::new();
    let mut ranges = [
        range(512, 64),
        range(256, 32),
        range(288, 32),
        range(900, 0),
    ];
    let descriptors = storage.descriptors();
    // SAFETY: storage exclusively supplies PAGE aligned, writable bytes and
    // outlives the table and all lookups. No page-content mapping is used.
    let mut table = unsafe { initialize(&mut ranges, &mut storage) }.unwrap();
    assert_eq!(table.covered_pages(), 128);
    for pfn in [0, 255, 320, 511, 576, usize::MAX] {
        assert!(table.lookup(pfn).is_none());
    }
    for (pfn, index) in [
        (256, 0),
        (287, 31),
        (288, 32),
        (319, 63),
        (512, 64),
        (575, 127),
    ] {
        let meta = table.lookup(pfn).unwrap();
        assert!(!meta.is_permanent());
        assert_eq!(
            meta as *const PageMeta as usize,
            descriptors.as_ptr() as usize + index * size_of::<PageMeta>()
        );
        assert!(core::ptr::eq(meta, table.lookup(pfn).unwrap()));
    }
    let identity = table.lookup(575).unwrap() as *const PageMeta;
    table.mark_permanent_range(575, 1).unwrap();
    assert!(table.lookup(575).unwrap().is_permanent());
    assert_eq!(identity, table.lookup(575).unwrap() as *const PageMeta);
    assert!(!table.lookup(574).unwrap().is_permanent());
}

#[def_test]
fn descriptor_array_size_rejects_overflow_and_out_of_bounds_storage() {
    let too_many_pages = isize::MAX as usize / size_of::<PageMeta>() + 1;
    assert_eq!(
        storage_bytes(too_many_pages),
        Err(PageMetaError::InvalidRange)
    );
    assert_eq!(storage_bytes(usize::MAX), Err(PageMetaError::InvalidRange));
}

#[def_test]
fn backend_capacity_is_checked_before_storage_is_touched() {
    let mut ranges =
        core::array::from_fn::<_, { CAPACITY + 1 }, _>(|index| range(2 * index + 1, 1));
    free_ranges::normalize(&mut ranges).unwrap();
    let mut storage = Storage::new();
    let mut mapped = false;
    // SAFETY: normalized modeled RAM is exclusive. The mapper would supply
    // retained writable fixture backing, but capacity failure must precede it.
    let result = unsafe {
        super::init(&mut ranges, &mut |_| {
            mapped = true;
            Ok(storage.descriptors().cast())
        })
    };
    assert!(matches!(result, Err(PageMetaError::CapacityExceeded)));
    assert!(!mapped);
    assert!(storage.0.iter().all(|&byte| byte == 0xa5));
}

#[def_test]
fn insufficient_descriptor_backing_is_rejected_before_any_write() {
    let mut storage = Storage::new();
    let pages = PAGE / size_of::<PageMeta>() + 1;
    let mut ranges = [range(1, pages)];
    // SAFETY: the supplied buffer is valid and retained. Insufficient capacity
    // for the layout is a checked error before any descriptor writes occur.
    let result = unsafe { initialize(&mut ranges, &mut storage) };
    assert!(matches!(result, Err(PageMetaError::InsufficientStorage)));
    assert!(storage.0.iter().all(|&byte| byte == 0xa5));
}

#[def_test]
fn invalid_reservations_leave_the_whole_directory_unchanged() {
    let mut storage = Storage::new();
    let mut ranges = [range(256, 8)];
    // SAFETY: storage is exclusive, properly aligned and large enough. The
    // table and any descriptor borrows are destroyed before storage.
    let mut table = unsafe { initialize(&mut ranges, &mut storage) }.unwrap();
    assert_eq!(
        table.mark_permanent_range(263, 2),
        Err(PageMetaError::NotCovered)
    );
    assert_eq!(
        table.mark_permanent_range(256, usize::MAX),
        Err(PageMetaError::InvalidRange)
    );
    assert_eq!(
        table.mark_permanent_range(255, 1),
        Err(PageMetaError::NotCovered)
    );
    for pfn in 256..264 {
        assert!(!table.lookup(pfn).unwrap().is_permanent());
    }
    table.mark_permanent_range(256, 8).unwrap();
    table.mark_permanent_range(256, 8).unwrap();
    assert!((256..264).all(|pfn| table.lookup(pfn).unwrap().is_permanent()));
}
