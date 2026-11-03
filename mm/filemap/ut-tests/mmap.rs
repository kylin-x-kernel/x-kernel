// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::paging::{MappingFlags, PageSize, PageTable};
use kvfs::MmapMapper;
use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};
use memspace::{MmObserver, VmRuntimeRef, backend::BackendOps};
use unittest::def_test;

use crate::{
    FileMappingMode,
    mmap::{FileMapper, FileMmapRequest},
};

fn file_mapper(name: &str, length: usize, offset: usize) -> FileMapper {
    let file = crate::test_support::page_cache_file(name);
    let aspace = Arc::new(ksync::Mutex::new(
        memspace::MmSpace::new_user_empty().expect("mm"),
    ));
    let observer = MmObserver::new(&aspace);
    let invalidate = observer.invalidate_handle();
    FileMapper::new_for_test(
        FileMmapRequest {
            start: VirtAddr::from_usize(0x1_0000),
            length,
            offset,
            page_size: PageSize::Size4K,
            flags: MappingFlags::READ | MappingFlags::WRITE,
            max_flags: MappingFlags::READ | MappingFlags::WRITE,
            file,
            mm_id: aspace.lock().mm_id(),
            observer,
            invalidate,
        },
        FileMappingMode::Shared,
    )
}

/// Devices multiplex buffers behind one fd offset, but the mapper must
/// translate that offset to an absolute physical range. Re-adding the offset
/// here used to make the linear backend map the wrong frame.
#[def_test]
fn physical_mapping_uses_absolute_backing_not_file_offset() {
    let length = 3 * PAGE_SIZE_4K;
    // The first DRM dumb buffer lives at this file offset.
    let mut mapper = file_mapper("absolute-physical-range", length, 8 * 1024 * 1024);
    let start = VirtAddr::from_usize(0x1_0000);
    let paddr = PhysAddr::from(0x8_0000);
    mapper
        .map_physical(PhysAddrRange::from_start_size(paddr, length))
        .expect("map physical range");

    let VmRuntimeRef::Linear(backend) = mapper.into_runtime_for_test().expect("linear runtime")
    else {
        panic!("physical mappings must use the linear backend");
    };
    let mut pgtbl = PageTable::try_new().expect("page table");
    {
        let mut pgtbl = pgtbl.modify();
        backend
            .map(
                VirtAddrRange::from_start_size(start, length),
                MappingFlags::READ | MappingFlags::WRITE,
                &mut pgtbl,
            )
            .expect("install linear mapping");
    }
    for page in 0..3 {
        let offset = page * PAGE_SIZE_4K;
        let va = VirtAddr::from_usize(start.as_usize() + offset);
        let expected = PhysAddr::from(paddr.as_usize() + offset);
        let (mapped, _, _) = pgtbl.query(va).expect("mapped page");
        assert_eq!(mapped, expected);
    }
}

/// A physical backing shorter than the requested VMA cannot satisfy the
/// mapping. Linux rejects this in the DRM mmap path instead of returning a
/// silently shortened VMA.
#[def_test]
fn physical_mapping_rejects_request_longer_than_backing() {
    let length = 3 * PAGE_SIZE_4K;
    let mut mapper = file_mapper("oversized-physical-range", length, 0);
    let paddr = PhysAddr::from(0x8_0000);

    assert!(matches!(
        mapper.map_physical(PhysAddrRange::from_start_size(paddr, 2 * PAGE_SIZE_4K)),
        Err(kvfs::VfsError::InvalidInput)
    ));
}
