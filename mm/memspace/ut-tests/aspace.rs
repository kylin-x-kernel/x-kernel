// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::{
    paging::{MappingFlags, PageSize, PagingError},
    trap::PageFaultFlags,
};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use unittest::def_test;
use vmobj::{
    FileObjectId, MappingView, MappingViewId, MappingViewKind, MappingViewRange,
    ObjectInvalidateRequest, ObjectViewHit, VmObjectId,
};

use super::*;
use crate::{PageFaultOutcome, vma::VmRuntimeRef};

fn new_test_aspace() -> AddrSpace {
    AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K * 64)
        .expect("test address space should be constructible")
}

fn map_shared(
    aspace: &mut AddrSpace,
    start: usize,
    size: usize,
    flags: MappingFlags,
) -> VmRuntimeRef {
    let start = VirtAddr::from(start);
    let backend = VmRuntimeRef::new_anon_shared(start, size, PageSize::Size4K)
        .expect("shared backend allocation should succeed");
    aspace
        .map(start, size, flags, false, backend.clone())
        .expect("mapping should succeed");
    backend
}

fn sample_request() -> ObjectInvalidateRequest {
    let view = MappingView::new(
        MappingViewId::from_raw(1),
        7,
        MappingViewRange {
            vma_start: 0x4000,
            vma_len: PAGE_SIZE_4K,
            object_start: 0,
            object_len: PAGE_SIZE_4K,
        },
        MappingViewKind::Shared,
    );
    ObjectInvalidateRequest::new(
        VmObjectId::File(FileObjectId::from_raw(11)),
        ObjectViewHit::new(view, 0, PAGE_SIZE_4K),
    )
}

#[def_test]
fn file_fault_capacity_failure_is_a_bus_error() {
    let start = VirtAddr::from(0x1000_0000);
    let mut aspace = MmSpace::new_empty_user(start, PAGE_SIZE_4K).unwrap();
    let flags = MappingFlags::READ | MappingFlags::USER;
    assert_eq!(
        aspace.finish_file_backend_fault(start, flags, Err(KError::StorageFull)),
        PageFaultOutcome::BusError
    );
    assert_eq!(
        aspace.finish_file_backend_fault(start, flags, Err(KError::NoMemory)),
        PageFaultOutcome::OutOfMemory
    );
}

#[def_test]
fn futex_backing_uses_mm_identity_for_private_mappings() {
    let start = VirtAddr::from_usize(0x4000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let mut first =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate first address space");
    first
        .map(
            start,
            PAGE_SIZE_4K,
            flags,
            false,
            VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
        )
        .expect("map private futex page");
    let second =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate second address space");

    assert_eq!(
        first
            .resolve_futex_backing(start.as_usize())
            .expect("resolve private futex"),
        FutexBacking::Private {
            mm_id: first.mm_id(),
            address: start.as_usize(),
        }
    );
    assert_ne!(first.mm_id(), second.mm_id());
}

#[def_test]
fn futex_backing_preserves_shared_object_offset_after_vma_trim() {
    let mut aspace = new_test_aspace();
    let start = VirtAddr::from_usize(0x5000);
    map_shared(
        &mut aspace,
        start.as_usize(),
        PAGE_SIZE_4K * 3,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    let address = start.as_usize() + PAGE_SIZE_4K * 2 + 4;
    let before = aspace
        .resolve_futex_backing(address)
        .expect("resolve shared futex before trim");
    aspace
        .unmap(start, PAGE_SIZE_4K)
        .expect("trim first mapping page");
    let after = aspace
        .resolve_futex_backing(address)
        .expect("resolve shared futex after trim");

    assert_eq!(before, after);
    assert!(matches!(
        after,
        FutexBacking::Shared {
            page_index: 2,
            offset_in_page: 4,
            ..
        }
    ));
}

#[def_test]
fn futex_backing_rejects_unaligned_or_unmapped_addresses() {
    let aspace = new_test_aspace();
    assert!(matches!(
        aspace.resolve_futex_backing(0x4001),
        Err(KError::InvalidInput)
    ));
    assert!(matches!(
        aspace.resolve_futex_backing(0x4000),
        Err(KError::BadAddress)
    ));
}

#[def_test]
fn invalidate_sink_removes_exact_enqueued_request() {
    let sink = InvalidateSink::default();
    let req = sample_request();

    let first = sink.enqueue(req.clone());
    let second = sink.enqueue(req.clone());

    assert!(sink.remove(second));

    let (remaining_id, remaining_req) = sink.pop_front().expect("first request must remain");
    assert_eq!(remaining_id, first);
    assert_eq!(remaining_req, req);
    assert!(sink.pop_front().is_none());
}

#[def_test]
fn fault_adapter_treats_retry_outcomes_as_handled() {
    assert!(MmSpace::page_fault_outcome_is_handled(
        PageFaultOutcome::Resolved
    ));
    assert!(MmSpace::page_fault_outcome_is_handled(
        PageFaultOutcome::Retry
    ));
    assert!(MmSpace::page_fault_outcome_is_handled(
        PageFaultOutcome::CowConflictRetry
    ));
    assert!(!MmSpace::page_fault_outcome_is_handled(
        PageFaultOutcome::AccessDenied
    ));
}

#[def_test]
fn madvise_dontneed_requires_fully_mapped_range() {
    let start = VirtAddr::from_usize(0x4000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let mut aspace =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4).expect("allocate test address space");
    aspace
        .map(
            start,
            PAGE_SIZE_4K,
            flags,
            false,
            VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
        )
        .expect("map first page");
    aspace
        .map(
            start + PAGE_SIZE_4K * 2,
            PAGE_SIZE_4K,
            flags,
            false,
            VmRuntimeRef::new_anon_private(
                start + PAGE_SIZE_4K * 2,
                khal::paging::PageSize::Size4K,
            ),
        )
        .expect("map third page");
    assert!(
        aspace
            .handle_page_fault(start, PageFaultFlags::WRITE)
            .is_resolved()
    );

    assert!(
        aspace.madvise_dontneed(start, PAGE_SIZE_4K * 3).is_err(),
        "range with an unmapped gap must not be partially discarded"
    );
    assert!(
        aspace.pgtbl.modify().query(start).is_ok(),
        "failed MADV_DONTNEED must leave existing PTEs intact"
    );
}

#[def_test]
fn overlapping_lazy_mapping_preserves_existing_mapping() {
    let mut aspace = new_test_aspace();
    let start = VirtAddr::from(0x5000usize);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    map_shared(&mut aspace, start.as_usize(), PAGE_SIZE_4K, flags);
    aspace
        .write(start, b"kept")
        .expect("seed the original mapping");
    let original_frame = aspace
        .pgtbl
        .query(start)
        .expect("original PTE must exist")
        .0;

    // The private backend maps lazily, so page-table overlap detection
    // cannot protect the VMA insertion or its object-view registration.
    let incoming = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    assert!(matches!(
        aspace.map(start, PAGE_SIZE_4K * 2, MappingFlags::READ, false, incoming),
        Err(KError::AlreadyExists)
    ));

    let mut contents = [0_u8; 4];
    aspace
        .read(start, &mut contents)
        .expect("original contents must remain readable");
    assert_eq!(contents, *b"kept");
    assert_eq!(
        aspace
            .pgtbl
            .query(start)
            .expect("original PTE must exist")
            .0,
        original_frame
    );
    assert!(aspace.can_access_range(start, PAGE_SIZE_4K, flags));
    assert_eq!(
        aspace
            .find_vma(start)
            .expect("original VMA must remain installed")
            .size(),
        PAGE_SIZE_4K
    );
    assert!(aspace.find_vma(start + PAGE_SIZE_4K).is_none());
    assert!(aspace.pgtbl.query(start + PAGE_SIZE_4K).is_err());
}

#[def_test]
fn test_mmap_resolve_addr_honors_fixed_and_no_replace_policies() {
    let mut aspace = new_test_aspace();
    map_shared(
        &mut aspace,
        0x5000,
        PAGE_SIZE_4K * 2,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    let any = aspace
        .mmap_resolve_addr(
            VirtAddr::from(0x5000usize),
            PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            AddrPolicy::Any,
        )
        .expect("allocator should find another free gap");
    assert_ne!(any, VirtAddr::from(0x5000usize));
    assert!(aspace.contains_range(any, PAGE_SIZE_4K));

    let err = aspace
        .mmap_resolve_addr(
            VirtAddr::from(0x5000usize),
            PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            AddrPolicy::FixedNoReplace,
        )
        .expect_err("occupied fixed-no-replace mapping must fail");
    assert!(matches!(err, KError::AlreadyExists));

    let fixed = aspace
        .mmap_resolve_addr(
            VirtAddr::from(0x5000usize),
            PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            AddrPolicy::Fixed,
        )
        .expect("fixed mappings should unmap and reuse the requested address");
    assert_eq!(fixed, VirtAddr::from(0x5000usize));
    assert!(aspace.find_vma(VirtAddr::from(0x5000usize)).is_none());
    assert!(aspace.find_vma(VirtAddr::from(0x6000usize)).is_some());
}

#[def_test]
fn test_map_protect_and_access_checks_follow_permissions() {
    let mut aspace = new_test_aspace();
    let start = VirtAddr::from(0x9000usize);

    map_shared(
        &mut aspace,
        start.as_usize(),
        PAGE_SIZE_4K * 2,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::READ));
    assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::WRITE));
    assert!(!aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::EXECUTE));

    aspace
        .protect(start, PAGE_SIZE_4K * 2, MappingFlags::READ)
        .expect("protection update should succeed");

    assert!(aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::READ));
    assert!(!aspace.can_access_range(start, PAGE_SIZE_4K * 2, MappingFlags::WRITE));
}

#[def_test]
fn test_extend_area_grows_existing_mapping_and_rejects_missing_start() {
    let mut aspace = new_test_aspace();
    let start = VirtAddr::from(0xd000usize);
    map_shared(
        &mut aspace,
        start.as_usize(),
        PAGE_SIZE_4K,
        MappingFlags::READ,
    );

    aspace
        .extend_area(start, PAGE_SIZE_4K)
        .expect("adjacent free space should allow extension");

    let area = aspace
        .find_vma(start)
        .expect("extended area should remain discoverable");
    assert_eq!(area.size(), PAGE_SIZE_4K * 2);

    let err = aspace
        .extend_area(VirtAddr::from(0x20000usize), PAGE_SIZE_4K)
        .expect_err("extending a missing area must fail");
    assert!(matches!(err, KError::NoMemory));
}

#[def_test]
fn test_move_pages_preserves_contents_and_clears_source_mapping() {
    let mut aspace = new_test_aspace();
    let src = VirtAddr::from(0x11000usize);
    let dst = VirtAddr::from(0x15000usize);
    map_shared(
        &mut aspace,
        src.as_usize(),
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    let payload = [1_u8, 2, 3, 4, 5];
    aspace
        .write(src + 37, &payload)
        .expect("mapped page should accept writes");

    aspace
        .move_pages(src, dst, PAGE_SIZE_4K, PageSize::Size4K)
        .expect("moving pages within one address space should succeed");

    let mut buf = [0_u8; 5];
    aspace
        .read(dst + 37, &mut buf)
        .expect("moved mapping should stay readable");
    assert_eq!(buf, payload);

    let err = aspace
        .read(src + 37, &mut buf)
        .expect_err("source mapping should be removed after move");
    assert!(matches!(err, KError::BadAddress));
}

#[def_test]
fn map_with_max_flags_allows_protection_raise_within_may_permissions() {
    let start = VirtAddr::from_usize(0x4000);
    let current = MappingFlags::USER;
    let max = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let raised = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let mut aspace =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate test address space");

    aspace
        .map_with_max_flags(
            start,
            PAGE_SIZE_4K,
            current,
            max,
            false,
            VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
        )
        .expect("map prot-none-style private region");

    aspace
        .protect(start, PAGE_SIZE_4K, raised)
        .expect("mprotect should allow permissions inside max flags");
    assert!(aspace.can_access_range(start, PAGE_SIZE_4K, raised));
}

#[def_test]
fn relocated_private_mapping_keeps_object_contents_after_source_metadata_drop() {
    let start = VirtAddr::from_usize(0x4000);
    let target = VirtAddr::from_usize(0x10000);
    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let len = PAGE_SIZE_4K * 2;
    let aspace = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 32)
            .expect("allocate relocation test address space"),
    ));

    let mut mm = aspace.lock();
    mm.map(
        start,
        len,
        flags,
        false,
        VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
    )
    .expect("map private source");
    mm.populate_area(start, PAGE_SIZE_4K, flags)
        .expect("populate source page");
    mm.write(start, b"abc").expect("seed source contents");

    let source = mm
        .resolve_mremap_source(start, len)
        .expect("resolve relocation source");
    mm.map_relocated_snapshot(&source, target, len, flags, &aspace)
        .expect("install relocated snapshot");
    mm.move_pages(start, target, len, khal::paging::PageSize::Size4K)
        .expect("move present pages");
    mm.drop_mapping_metadata(start, len)
        .expect("retire old metadata");

    assert!(!mm.can_access_range(start, PAGE_SIZE_4K, flags));

    mm.invalidate_present(target, PAGE_SIZE_4K)
        .expect("drop present destination PTE");
    mm.populate_area(target, PAGE_SIZE_4K, flags)
        .expect("refault destination page");

    let mut buf = [0u8; 3];
    mm.read(target, &mut buf)
        .expect("read relocated contents after refault");
    assert_eq!(&buf, b"abc");
}

#[def_test]
fn mm_pin_does_not_keep_user_mappings_alive_after_last_user_release() {
    let start = VirtAddr::from_usize(0x4000);
    let aspace = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate test address space"),
    ));
    let user = MmSpace::acquire_user(aspace.clone()).expect("new mm should accept user");
    let pin = user.pin();

    assert!(pin.is_user_alive());
    assert!(
        user.release_and_clear_if_last(),
        "last active user should tear down mappings"
    );
    assert!(
        pin.try_upgrade_user().is_none(),
        "object pin must not resurrect a torn-down user address space"
    );
    let pinned_mm_id = pin.address_space().lock().mm_id();
    let original_mm_id = aspace.lock().mm_id();
    assert_eq!(
        pinned_mm_id, original_mm_id,
        "pin should still keep the mm object observable"
    );
}

#[def_test]
fn clear_tolerates_zapped_linear_mapping() {
    let start = VirtAddr::from_usize(0x4000);
    let flags = MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER;
    let frame = crate::backend::alloc_frame(true, PageSize::Size4K).expect("allocate linear frame");
    let mut aspace =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 2).expect("allocate test address space");

    aspace
        .map_linear(start, frame, PAGE_SIZE_4K, flags)
        .expect("map linear page");
    assert!(aspace.page_table().query(start).is_ok());
    aspace
        .invalidate_present(start, PAGE_SIZE_4K)
        .expect("zap linear page while retaining VMA metadata");
    assert_eq!(
        aspace.page_table().query(start),
        Err(PagingError::NotMapped)
    );

    aspace.clear();
    crate::backend::dealloc_frame(frame, PageSize::Size4K);
}
