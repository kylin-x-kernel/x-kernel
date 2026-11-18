// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;

use kalloc_types::{AllocFlags, KBox};
use khal::{
    paging::{MappingFlags, PageSize, PagingError},
    trap::PageFaultFlags,
};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use unittest::def_test;
use vmobj::{
    FileObjectId, MappingView, MappingViewId, MappingViewKind, MappingViewRange,
    ObjectInvalidateRequest, ObjectViewHit, VmObjectId,
};

use super::*;
use crate::{
    PageFaultOutcome,
    backend::{FaultCompletion, PopulateHook},
    vma::VmRuntimeRef,
};

struct PopulateCapture(Arc<AtomicUsize>);

impl Drop for PopulateCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn populate_hook_consumes_capture_once_and_can_only_be_taken_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut address_space = new_test_aspace();
    for flags in [AllocFlags::KERNEL, AllocFlags::NOWAIT] {
        let capture = PopulateCapture(drops.clone());
        let hits = calls.clone();
        let hook = PopulateHook::new_with_flags(
            move |space| {
                assert_eq!(space.base(), VirtAddr::from_usize(0x1000));
                hits.fetch_add(1, Ordering::Relaxed);
                drop(capture);
            },
            flags,
        )
        .unwrap();
        let mut completion = FaultCompletion::from_populate((1, Some(hook)));
        let hook = completion.take_post_action().unwrap();
        assert!(completion.take_post_action().is_none());
        hook.call(&mut address_space);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[def_test]
fn cancelled_and_rejected_populate_hooks_release_captures_without_calling() {
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = PopulateCapture(drops.clone());
    let hits = calls.clone();
    let hook = PopulateHook::new(move |_| {
        hits.fetch_add(1, Ordering::Relaxed);
        drop(capture);
    })
    .unwrap();
    drop(FaultCompletion::from_populate((1, Some(hook))));
    let capture = PopulateCapture(drops.clone());
    let hits = calls.clone();
    let rejected = PopulateHook::new_with_flags(
        move |_| {
            hits.fetch_add(1, Ordering::Relaxed);
            drop(capture);
        },
        AllocFlags::from_bits_retain(1 << 31),
    );
    assert!(matches!(rejected, Err(KError::InvalidInput)));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[def_test]
fn backend_fault_completion_runs_prepared_hook_and_preserves_oom_outcome() {
    let calls = Arc::new(AtomicUsize::new(0));
    let hits = calls.clone();
    let mut address_space = new_test_aspace();
    let hook = PopulateHook::new_nowait(move |_| {
        hits.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let start = address_space.base();
    let result = address_space.finish_backend_fault(
        start,
        MappingFlags::READ,
        Ok(FaultCompletion::from_populate((1, Some(hook)))),
    );
    assert_eq!(result, PageFaultOutcome::Resolved);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        address_space.finish_backend_fault(start, MappingFlags::READ, Err(KError::NoMemory)),
        PageFaultOutcome::OutOfMemory
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

fn new_test_aspace() -> AddrSpace {
    AddrSpace::new_empty_kernel(VirtAddr::from(0x1000usize), PAGE_SIZE_4K * 64)
        .expect("test address space should be constructible")
}

#[def_test]
fn linear_mapping_defers_its_kernel_invalidation() {
    let mut aspace = new_test_aspace();
    let start = VirtAddr::from(0x1000usize);
    let paddr = PhysAddr::from(0x20_0000usize);

    // Architectures whose page-table metadata needs no invalidation for an
    // invalid-to-valid mapping (aarch64's fresh-map fence) publish the PTE
    // without a deferred flush; the others must defer it.
    let map_deferred = aspace
        .map_linear_deferred(
            start,
            paddr,
            PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
        )
        .expect("linear mapping should succeed");
    // The PTE is published before the remote shootdown completes.
    assert!(aspace.page_table().query(start).is_ok());
    if let Some(deferred) = map_deferred {
        assert_eq!(deferred.complete(), Ok(()));
    }

    // Removing a valid translation always needs an invalidation, so every
    // architecture must defer the unmap.
    let deferred = aspace
        .unmap_linear_deferred(start, PAGE_SIZE_4K)
        .expect("linear unmap should succeed")
        .expect("a kernel unmap should defer its invalidation");
    assert!(aspace.page_table().query(start).is_err());
    assert_eq!(deferred.complete(), Ok(()));
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
        aspace.finish_file_backend_fault(start, flags, Err(KError::OperationNotPermitted)),
        PageFaultOutcome::BusError
    );
    assert_eq!(
        aspace.finish_file_backend_fault(start, flags, Err(KError::WouldBlock)),
        PageFaultOutcome::Retry
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
fn private_fork_after_vma_split_keeps_child_cow_object_offset() {
    let start = VirtAddr::from_usize(0x60_0000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let mut parent = MmSpace::new_empty_user(start, PAGE_SIZE_4K * 8)
        .expect("allocate parent address space");
    parent
        .map(
            start,
            PAGE_SIZE_4K * 3,
            flags,
            false,
            VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
        )
        .expect("map private range");
    let second_page = start + PAGE_SIZE_4K;
    assert!(parent
        .handle_page_fault(second_page, PageFaultFlags::WRITE)
        .is_resolved());
    parent
        .protect(
            start,
            PAGE_SIZE_4K,
            MappingFlags::USER | MappingFlags::READ,
        )
        .expect("split first VMA page");

    let child = parent.try_clone().expect("fork split private mapping");
    assert!(child
        .lock()
        .handle_page_fault(second_page, PageFaultFlags::WRITE)
        .is_resolved());
}

#[def_test]
fn fork_and_unmap_across_vmas_with_holes_preserves_remaining_mappings() {
    let start = VirtAddr::from_usize(0x70_0000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let mut parent =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 8).expect("allocate parent address space");
    // Three mappings with holes between them, so an unmapping request spans
    // partial VMAs at both ends, a whole VMA, and the holes in between.
    for page in [0, 3, 6] {
        let address = start + page * PAGE_SIZE_4K;
        parent
            .map(
                address,
                PAGE_SIZE_4K * 2,
                flags,
                false,
                VmRuntimeRef::new_anon_private(address, PageSize::Size4K),
            )
            .expect("map private range");
    }

    let child = parent.try_clone().expect("fork spanning VMAs");
    parent
        .unmap(start + PAGE_SIZE_4K, PAGE_SIZE_4K * 6)
        .expect("unmap across VMAs and holes");

    for page in 0..8 {
        let address = start + page * PAGE_SIZE_4K;
        assert_eq!(
            parent.find_vma(address).is_some(),
            page == 0 || page == 7,
            "parent page {page}"
        );
        assert_eq!(
            child.lock().find_vma(address).is_some(),
            page != 2 && page != 5,
            "child page {page}"
        );
    }
}

#[def_test]
fn unmap_entirely_inside_a_hole_leaves_the_neighbouring_mappings() {
    let start = VirtAddr::from_usize(0x70_0000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    let mut parent =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 8).expect("allocate parent address space");
    parent
        .map(
            start,
            PAGE_SIZE_4K * 2,
            flags,
            false,
            VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
        )
        .expect("map the lower range");
    let upper = start + PAGE_SIZE_4K * 6;
    parent
        .map(
            upper,
            PAGE_SIZE_4K * 2,
            flags,
            false,
            VmRuntimeRef::new_anon_private(upper, PageSize::Size4K),
        )
        .expect("map the upper range");

    // The request is shorter than the hole it sits in, so the next VMA starts
    // beyond its end address. Unmapping it touches no mapping at all.
    let hole = start + PAGE_SIZE_4K * 3;
    parent
        .unmap(hole, PAGE_SIZE_4K)
        .expect("unmap a hole without neighbours in range");

    assert_eq!(
        parent
            .find_vma(start + PAGE_SIZE_4K)
            .map(|vma| vma.start())
            .unwrap_or_else(|| VirtAddr::from_usize(0)),
        start
    );
    assert_eq!(
        parent
            .find_vma(upper + PAGE_SIZE_4K)
            .map(|vma| vma.start())
            .unwrap_or_else(|| VirtAddr::from_usize(0)),
        upper
    );
    assert!(
        parent.find_vma(hole).is_none(),
        "the hole stays unmapped"
    );
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

struct ViewInvalidationFixture {
    mm: Arc<Mutex<MmSpace>>,
    handle: InvalidateHandle,
    request: ObjectInvalidateRequest,
}

fn view_invalidation_fixture() -> ViewInvalidationFixture {
    let vma_start = 0x40_0000usize;
    let start = VirtAddr::from(vma_start);
    let mm = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4)
            .expect("test address space should be constructible"),
    ));
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    let object = match runtime.backing_info().kind() {
        VmBackingKind::AnonymousPrivate { object } => object,
        _ => panic!("fixture runtime must carry an anonymous private backing object"),
    };
    mm.lock()
        .map(
            start,
            PAGE_SIZE_4K,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
            false,
            runtime,
        )
        .expect("mapping should succeed");
    let handle = mm.lock().invalidate_handle(&mm);
    let view = MappingView::new(
        MappingViewId::from_raw(1),
        mm.lock().mm_id(),
        MappingViewRange {
            vma_start: vma_start as u64,
            vma_len: PAGE_SIZE_4K,
            object_start: 0,
            object_len: PAGE_SIZE_4K,
        },
        MappingViewKind::Private,
    );
    let hit = ObjectViewHit::new(view, 0, PAGE_SIZE_4K);
    ViewInvalidationFixture {
        mm,
        handle,
        request: ObjectInvalidateRequest::new(object, hit),
    }
}

#[def_test]
fn live_handle_applies_present_pte_and_reports_success() {
    let fixture = view_invalidation_fixture();
    let page = VirtAddr::from_usize(0x40_0000);
    assert!(
        fixture
            .mm
            .lock()
            .handle_page_fault(page, PageFaultFlags::WRITE)
            .is_resolved()
    );
    assert!(fixture.mm.lock().page_table().query(page).is_ok());

    assert!(
        fixture
            .handle
            .try_apply(&fixture.request)
            .expect("idle address space applies the request")
    );

    assert!(fixture.mm.lock().page_table().query(page).is_err());
    assert!(fixture.mm.lock().invalidate_sink.pop_front().is_none());
}

#[def_test]
fn busy_address_space_reports_pending() {
    let fixture = view_invalidation_fixture();
    let guard = fixture.mm.lock();

    assert!(
        !fixture
            .handle
            .try_apply(&fixture.request)
            .expect("busy probe does not fail")
    );

    drop(guard);
}

#[def_test]
fn deferred_handle_keeps_request_queued_until_drain() {
    let fixture = view_invalidation_fixture();
    let handle = fixture.mm.lock().deferred_invalidate_handle();

    assert!(
        !handle
            .try_apply(&fixture.request)
            .expect("deferred probe does not fail")
    );
    assert!(
        !handle
            .submit(fixture.request.clone())
            .expect("deferred submit does not fail")
    );

    let (id, queued) = fixture
        .mm
        .lock()
        .invalidate_sink
        .pop_front()
        .expect("deferred request must stay queued");
    assert_eq!(queued, fixture.request);
    fixture.mm.lock().invalidate_sink.requeue_back(id, queued);
    fixture.mm.lock().drain_pending_invalidations();
    assert!(fixture.mm.lock().invalidate_sink.pop_front().is_none());
}

#[def_test]
fn destroyed_address_space_reports_complete() {
    let start = VirtAddr::from(0x40_0000usize);
    let mm = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4)
            .expect("test address space should be constructible"),
    ));
    let handle = mm.lock().invalidate_handle(&mm);
    drop(mm);

    assert!(
        handle
            .try_apply(&sample_request())
            .expect("a destroyed address space is already complete")
    );
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
fn mapped_size_sums_vma_lengths_not_address_space_span() {
    let mut aspace = new_test_aspace();
    assert_eq!(aspace.mapped_size(), 0);
    assert_eq!(aspace.size(), PAGE_SIZE_4K * 64);

    map_shared(
        &mut aspace,
        0x5000,
        PAGE_SIZE_4K * 2,
        MappingFlags::READ | MappingFlags::WRITE,
    );
    map_shared(
        &mut aspace,
        0x9000,
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::WRITE,
    );

    assert_eq!(aspace.vmas().count(), 2);
    assert_eq!(aspace.mapped_size(), PAGE_SIZE_4K as u64 * 3);
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
fn sparse_move_preserves_holes_and_physical_identities() {
    let mut aspace = new_test_aspace();
    let src = VirtAddr::from(0x2000usize);
    let dst = VirtAddr::from(0x20000usize);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let last_offset = PAGE_SIZE_4K * 7;
    for address in [src, src + last_offset] {
        map_shared(&mut aspace, address.as_usize(), PAGE_SIZE_4K, flags);
    }
    let first = aspace.pgtbl.query(src).unwrap();
    let last = aspace.pgtbl.query(src + last_offset).unwrap();
    aspace.move_pages(src, dst, PAGE_SIZE_4K * 8, PageSize::Size4K).unwrap();
    assert_eq!(aspace.pgtbl.query(dst).unwrap(), first);
    assert_eq!(aspace.pgtbl.query(dst + last_offset).unwrap(), last);
    for offset in [0, PAGE_SIZE_4K, last_offset] {
        assert!(matches!(aspace.pgtbl.query(src + offset), Err(PagingError::NotMapped)));
    }
    assert!(matches!(aspace.pgtbl.query(dst + PAGE_SIZE_4K), Err(PagingError::NotMapped)));
}

#[def_test]
fn sparse_move_conflict_rolls_back_only_prepared_destinations() {
    let mut aspace = new_test_aspace();
    let src = VirtAddr::from(0x2000usize);
    let dst = VirtAddr::from(0x20000usize);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let last_offset = PAGE_SIZE_4K * 2;
    for address in [src, src + last_offset, dst + last_offset] {
        map_shared(&mut aspace, address.as_usize(), PAGE_SIZE_4K, flags);
    }
    let first = aspace.pgtbl.query(src).unwrap();
    let last = aspace.pgtbl.query(src + last_offset).unwrap();
    let conflict = aspace.pgtbl.query(dst + last_offset).unwrap();
    assert_eq!(
        aspace.move_pages(src, dst, PAGE_SIZE_4K * 3, PageSize::Size4K),
        Err(KError::InvalidInput)
    );
    assert_eq!(aspace.pgtbl.query(src).unwrap(), first);
    assert_eq!(aspace.pgtbl.query(src + last_offset).unwrap(), last);
    assert_eq!(aspace.pgtbl.query(dst + last_offset).unwrap(), conflict);
    for address in [src + PAGE_SIZE_4K, dst, dst + PAGE_SIZE_4K] {
        assert!(matches!(aspace.pgtbl.query(address), Err(PagingError::NotMapped)));
    }
}

#[def_test(serial)]
fn move_pages_keeps_managed_storage_after_source_retirement() {
    use kalloc::{UsageKind, alloc_page_ref, global_allocator};

    let before = global_allocator().usages().get(UsageKind::PageCache);
    let mut aspace = new_test_aspace();
    let src = VirtAddr::from(0x2000usize);
    let dst = VirtAddr::from(0x20000usize);
    let page = alloc_page_ref(UsageKind::PageCache).unwrap();
    let paddr = page.phys_addr();
    let mut edit = aspace.pgtbl.modify();
    // The source PTE is the sole physical owner, as after cache/token detachment.
    edit.map(src, page, MappingFlags::READ).unwrap();
    edit.try_finish().unwrap();
    drop(edit);

    aspace.move_pages(src, dst, PAGE_SIZE_4K * 3, PageSize::Size4K).unwrap();
    assert!(matches!(aspace.pgtbl.query(src), Err(PagingError::NotMapped)));
    assert!(matches!(aspace.pgtbl.query(dst + PAGE_SIZE_4K), Err(PagingError::NotMapped)));
    assert_eq!(aspace.pgtbl.query(dst).unwrap().0, paddr);
    assert_eq!(
        global_allocator().usages().get(UsageKind::PageCache),
        before + PAGE_SIZE_4K,
        "the destination must own storage after the source hold retires"
    );
    let mut edit = aspace.pgtbl.modify();
    edit.unmap(dst).unwrap();
    assert_eq!(global_allocator().usages().get(UsageKind::PageCache), before + PAGE_SIZE_4K);
    edit.try_finish().unwrap();
    assert_eq!(global_allocator().usages().get(UsageKind::PageCache), before);
}

#[def_test(serial)]
fn move_pages_conflict_retires_managed_destinations_without_consuming_sources() {
    use kalloc::{UsageKind, alloc_page_ref, global_allocator};

    let before = global_allocator().usages().get(UsageKind::PageCache);
    let mut aspace = new_test_aspace();
    let src = VirtAddr::from(0x2000usize);
    let dst = VirtAddr::from(0x20000usize);
    let offset = PAGE_SIZE_4K * 2;
    let mut edit = aspace.pgtbl.modify();
    for address in [src, src + offset, dst + offset] {
        edit.map(address, alloc_page_ref(UsageKind::PageCache).unwrap(), MappingFlags::READ).unwrap();
    }
    edit.try_finish().unwrap();
    drop(edit);
    let first = aspace.pgtbl.query_entry(src).unwrap();
    let last = aspace.pgtbl.query_entry(src + offset).unwrap();
    let conflict = aspace.pgtbl.query_entry(dst + offset).unwrap();

    assert_eq!(
        aspace.move_pages(src, dst, PAGE_SIZE_4K * 3, PageSize::Size4K),
        Err(KError::InvalidInput)
    );
    assert_eq!(aspace.pgtbl.query_entry(src).unwrap(), first);
    assert_eq!(aspace.pgtbl.query_entry(src + offset).unwrap(), last);
    assert_eq!(aspace.pgtbl.query_entry(dst + offset).unwrap(), conflict);
    assert!(matches!(aspace.pgtbl.query(dst), Err(PagingError::NotMapped)));
    assert_eq!(global_allocator().usages().get(UsageKind::PageCache), before + PAGE_SIZE_4K * 3);
    let mut edit = aspace.pgtbl.modify();
    // If rollback consumed a source hold or leaked the prepared alias, final
    // retirement would either underflow or leave an allocated page behind.
    for address in [src, src + offset, dst + offset] {
        edit.unmap(address).unwrap();
    }
    edit.try_finish().unwrap();
    assert_eq!(global_allocator().usages().get(UsageKind::PageCache), before);
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
    mm.relocate_mapping(&source, target, len, false, &aspace)
        .expect("relocate private snapshot");

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
fn relocated_shared_mapping_preserves_frames_and_refaults_at_destination() {
    let start = VirtAddr::from_usize(0x4000);
    let target = VirtAddr::from_usize(0x10000);
    let len = PAGE_SIZE_4K * 2;
    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let aspace = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 32)
            .expect("allocate shared relocation address space"),
    ));
    let mut mm = aspace.lock();
    map_shared(&mut mm, start.as_usize(), len, flags);
    mm.write(start, &[0x48]).expect("seed first shared page");
    mm.write(start + PAGE_SIZE_4K, &[0x79]).expect("seed second shared page");
    let frames = [
        mm.page_table().query(start).unwrap().0,
        mm.page_table().query(start + PAGE_SIZE_4K).unwrap().0,
    ];
    let source = mm.resolve_mremap_source(start, len).expect("shared source snapshot");
    mm.relocate_mapping(&source, target, len, false, &aspace)
        .expect("relocate eagerly populated shared runtime");
    assert!(mm.find_vma(start).is_none());
    for (page, frame) in frames.iter().enumerate() {
        assert!(mm.page_table().query(start + page * PAGE_SIZE_4K).is_err());
        assert_eq!(mm.page_table().query(target + page * PAGE_SIZE_4K).unwrap().0, *frame);
    }
    mm.invalidate_present(target, len).expect("drop destination shared PTEs");
    mm.populate_area(target, len, flags).expect("refault relocated shared pages");
    for (page, expected) in [0x48, 0x79].into_iter().enumerate() {
        let mut byte = [0];
        mm.read(target + page * PAGE_SIZE_4K, &mut byte).expect("read relocated shared payload");
        assert_eq!(byte, [expected]);
        assert_eq!(mm.page_table().query(target + page * PAGE_SIZE_4K).unwrap().0, frames[page]);
    }
    mm.unmap(target, len).expect("cleanup relocated shared mapping");
}

#[def_test]
fn shared_relocation_rejects_growth_before_destination_publication() {
    let start = VirtAddr::from_usize(0x4000);
    let target = VirtAddr::from_usize(0x10000);
    let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let aspace = Arc::new(Mutex::new(
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 32)
            .expect("allocate rejected shared-growth address space"),
    ));
    let mut mm = aspace.lock();
    map_shared(&mut mm, start.as_usize(), PAGE_SIZE_4K, flags);
    mm.write(start, &[0x53]).expect("seed shared source");
    let frame = mm.page_table().query(start).unwrap().0;
    let source = mm.resolve_mremap_source(start, PAGE_SIZE_4K).expect("shared source snapshot");
    assert_eq!(
        mm.relocate_mapping(&source, target, PAGE_SIZE_4K * 3, false, &aspace),
        Err(KError::InvalidInput),
        "fixed shared backing cannot supply a larger runtime range",
    );
    assert!(mm.find_vma(target).is_none());
    assert_eq!(mm.page_table().query(start).unwrap().0, frame);
    let mut byte = [0];
    mm.read(start, &mut byte).expect("rejected growth must preserve source");
    assert_eq!(byte, [0x53]);
    mm.unmap(start, PAGE_SIZE_4K).expect("cleanup rejected shared growth");
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
fn clear_removes_present_and_zapped_linear_mappings() {
    let start = VirtAddr::from_usize(0x4000);
    let present = start + 2 * PAGE_SIZE_4K;
    let flags = MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::USER;
    let frame = crate::backend::alloc_frame(true, PageSize::Size4K).expect("allocate linear frame");
    let present_frame =
        crate::backend::alloc_frame(true, PageSize::Size4K).expect("allocate second linear frame");
    let mut aspace =
        MmSpace::new_empty_user(start, PAGE_SIZE_4K * 3).expect("allocate test address space");

    aspace
        .map_linear(start, frame, PAGE_SIZE_4K, flags)
        .expect("map linear page");
    aspace
        .map_linear(present, present_frame, PAGE_SIZE_4K, flags)
        .expect("map second linear page");
    assert!(aspace.page_table().query(start).is_ok());
    aspace
        .invalidate_present(start, PAGE_SIZE_4K)
        .expect("zap linear page while retaining VMA metadata");
    assert_eq!(
        aspace.page_table().query(start),
        Err(PagingError::NotMapped)
    );

    aspace.clear();
    assert!(aspace.find_vma(start).is_none());
    assert!(aspace.find_vma(present).is_none());
    assert_eq!(aspace.page_table().query(present), Err(PagingError::NotMapped));
    crate::backend::dealloc_frame(frame, PageSize::Size4K);
    crate::backend::dealloc_frame(present_frame, PageSize::Size4K);
}

#[def_test]
fn synchronous_invalidation_is_complete_after_mm_destruction() {
    let mm = Arc::new(Mutex::new(MmSpace::new_user_empty().unwrap()));
    let handle = mm.lock().invalidate_handle(&mm);
    drop(mm);
    assert_eq!(handle.try_apply(&sample_request()), Ok(true));
}

// No PTEs are needed: these fixtures exercise VMA ownership and sync dispatch.
struct MsyncTestRuntime {
    backing: crate::VmBackingInfo,
    sync: KBox<dyn Fn(core::ops::Range<u64>) -> KResult + Send + Sync>,
}

impl crate::VmRuntimeOps for MsyncTestRuntime {
    fn backing_info(&self) -> crate::VmBackingInfo {
        self.backing
    }

    fn map(
        &self,
        _: memaddr::VirtAddrRange,
        _: MappingFlags,
        _: &mut khal::paging::PageTableMut,
    ) -> KResult {
        Ok(())
    }

    fn unmap(&self, _: memaddr::VirtAddrRange, _: &mut khal::paging::PageTableMut) -> KResult {
        Ok(())
    }

    fn on_protect(
        &self,
        _: memaddr::VirtAddrRange,
        flags: MappingFlags,
        _: &mut khal::paging::PageTableMut,
    ) -> KResult<MappingFlags> {
        Ok(flags)
    }

    fn msync(&self, range: core::ops::Range<u64>) -> KResult {
        (self.sync)(range)
    }

    fn handle_fault(
        &self,
        _: crate::FaultContext,
        _: MappingFlags,
        _: &mut khal::paging::PageTableMut,
    ) -> crate::backend::FaultCompletionResult {
        Err(KError::BadAddress)
    }

    fn relocate_for_mremap(
        &self,
        _: memaddr::VirtAddrRange,
        _: VirtAddr,
        _: usize,
        _: u64,
        _: &Arc<Mutex<MmSpace>>,
        _: Option<crate::InvalidateHandle>,
    ) -> KResult<Arc<dyn crate::VmRuntimeOps>> {
        Err(KError::OperationNotSupported)
    }

    fn clone_for_fork(
        &self,
        _: memaddr::VirtAddrRange,
        _: MappingFlags,
        _: &mut khal::paging::PageTableMut,
        _: &mut khal::paging::PageTableMut,
        _: crate::ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn crate::VmRuntimeOps>> {
        Err(KError::OperationNotSupported)
    }
}

fn map_msync_file(
    mm: &mut MmSpace,
    start: usize,
    pages: usize,
    offset: u64,
    shared: bool,
    sync: impl Fn(core::ops::Range<u64>) -> KResult + Send + Sync + 'static,
) {
    let object = VmObjectId::File(FileObjectId::from_raw(start as u64));
    let backing = crate::VmBackingInfo::new(
        if shared {
            VmBackingKind::FileShared { object }
        } else {
            VmBackingKind::FilePrivate {
                file_object: object,
                anon_object: VmObjectId::Anon(vmobj::AnonObjectId::from_raw(start as u64)),
                anon_lineage: anon::AnonPrivateObject::new_root().lineage(),
            }
        },
        PageSize::Size4K,
    );
    let runtime = Arc::new(MsyncTestRuntime {
        backing,
        sync: KBox::new_nowait(sync).unwrap(),
    });
    let runtime = if shared {
        VmRuntimeRef::new_file_shared(runtime)
    } else {
        VmRuntimeRef::new_file_private(runtime)
    };
    let vma = VmArea::new(
        VirtAddr::from_usize(start),
        pages * PAGE_SIZE_4K,
        MappingFlags::READ,
        MappingFlags::READ,
        backing,
        offset / PAGE_SIZE_4K as u64,
        Some(crate::FileMappingInfo {
            offset,
            inode: start as u64,
            path: None,
        }),
    );
    mm.map_runtime_vma(vma, false, runtime).unwrap();
}

#[def_test]
fn msync_unlocks_for_io_and_relooks_up_after_concurrent_mapping_replacement() {
    let mm = Arc::new(Mutex::new(new_test_aspace()));
    let owner = Arc::downgrade(&mm);
    let calls = Arc::new(Mutex::new(alloc::vec::Vec::new()));
    let seen = calls.clone();
    map_msync_file(
        &mut mm.lock(),
        0x1000,
        2,
        2 * PAGE_SIZE_4K as u64,
        true,
        move |range| {
            seen.lock().push(range);
            let mm = owner.upgrade().unwrap();
            let mut guard = mm
                .try_lock()
                .expect("msync must release MM serialization during persistence");
            // Retire even the current VMA; its file runtime must survive this sync.
            guard
                .unmap(VirtAddr::from_usize(0x1000), 3 * PAGE_SIZE_4K)
                .unwrap();
            let next_seen = seen.clone();
            let next_owner = Arc::downgrade(&mm);
            map_msync_file(
                &mut guard,
                0x3000,
                1,
                8 * PAGE_SIZE_4K as u64,
                true,
                move |range| {
                    assert!(next_owner.upgrade().unwrap().try_lock().is_some());
                    next_seen.lock().push(range);
                    Ok(())
                },
            );
            Ok(())
        },
    );
    map_msync_file(&mut mm.lock(), 0x3000, 1, 0, true, |_| {
        panic!("stale VMA must not be synchronized")
    });
    assert_eq!(
        MmSpace::msync_range(
            &mm,
            VirtAddr::from_usize(0x2000),
            2 * PAGE_SIZE_4K,
            crate::MsyncPolicy::new(crate::MsyncWriteback::Sync)
        ),
        Ok(())
    );
    assert_eq!(
        *calls.lock(),
        alloc::vec![
            3 * PAGE_SIZE_4K as u64..4 * PAGE_SIZE_4K as u64,
            8 * PAGE_SIZE_4K as u64..9 * PAGE_SIZE_4K as u64
        ]
    );
}

#[def_test]
fn msync_preserves_hole_error_precedence_and_skips_non_sync_and_private_mappings() {
    let mm = Mutex::new(new_test_aspace());
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    map_msync_file(&mut mm.lock(), 0x2000, 1, 0, true, move |_| {
        seen.fetch_add(1, Ordering::Relaxed);
        Err(KError::Io)
    });
    for mode in [
        crate::MsyncWriteback::Async,
        crate::MsyncWriteback::Unspecified,
    ] {
        assert_eq!(
            MmSpace::msync_range(
                &mm,
                VirtAddr::from_usize(0x1000),
                2 * PAGE_SIZE_4K,
                crate::MsyncPolicy::new(mode)
            ),
            Err(KError::NoMemory)
        );
        assert_eq!(
            MmSpace::msync_range(
                &mm,
                VirtAddr::from_usize(0x2000),
                PAGE_SIZE_4K,
                crate::MsyncPolicy::new(mode)
            ),
            Ok(())
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let sync = crate::MsyncPolicy::new(crate::MsyncWriteback::Sync);
    assert_eq!(
        MmSpace::msync_range(&mm, VirtAddr::from_usize(0x1000), 2 * PAGE_SIZE_4K, sync),
        Err(KError::Io)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    map_msync_file(&mut mm.lock(), 0x3000, 1, 0, false, |_| {
        panic!("private mappings must not call fsync")
    });
    assert_eq!(
        MmSpace::msync_range(&mm, VirtAddr::from_usize(0x3000), PAGE_SIZE_4K, sync),
        Ok(())
    );
    assert_eq!(
        MmSpace::msync_range(&mm, VirtAddr::from_usize(0x3000), 2 * PAGE_SIZE_4K, sync),
        Err(KError::NoMemory)
    );
    let seen = calls.clone();
    map_msync_file(&mut mm.lock(), 0x5000, 1, 0, true, move |_| {
        seen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    });
    assert_eq!(
        MmSpace::msync_range(&mm, VirtAddr::from_usize(0x4000), 2 * PAGE_SIZE_4K, sync),
        Err(KError::NoMemory)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

struct MsyncFileRelease {
    mm: alloc::sync::Weak<Mutex<MmSpace>>,
    releases: Arc<AtomicUsize>,
}

impl Drop for MsyncFileRelease {
    fn drop(&mut self) {
        let mm = self.mm.upgrade().unwrap();
        assert!(
            mm.try_lock().is_some(),
            "final file release must run outside MM serialization"
        );
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn msync_releases_the_last_file_reference_before_relocking_for_the_next_vma() {
    let mm = Arc::new(Mutex::new(new_test_aspace()));
    let releases = Arc::new(AtomicUsize::new(0));
    let file = MsyncFileRelease {
        mm: Arc::downgrade(&mm),
        releases: releases.clone(),
    };
    map_msync_file(&mut mm.lock(), 0x1000, 1, 0, true, move |_| {
        let mm = file.mm.upgrade().unwrap();
        mm.lock()
            .unmap(VirtAddr::from_usize(0x1000), PAGE_SIZE_4K)
            .unwrap();
        assert_eq!(file.releases.load(Ordering::Relaxed), 0);
        // Keep the whole final-release fixture in the runtime's capture.
        let _file = &file;
        Ok(())
    });
    map_msync_file(&mut mm.lock(), 0x2000, 1, 0, false, |_| {
        panic!("private sync")
    });
    assert_eq!(
        MmSpace::msync_range(
            &mm,
            VirtAddr::from_usize(0x1000),
            2 * PAGE_SIZE_4K,
            crate::MsyncPolicy::new(crate::MsyncWriteback::Sync)
        ),
        Ok(())
    );
    assert_eq!(releases.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn default_overcommit_rejects_oversized_private_commitments_but_preserves_lazy_noreserve() {
    let baseline = crate::committed_pages();
    let size = (kalloc::global_allocator().total_pages() + 1) * PAGE_SIZE_4K;
    let start = VirtAddr::from_usize(0x1000);
    let mut space = AddrSpace::new_empty_kernel(start, size + PAGE_SIZE_4K).unwrap();
    let writable = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    assert_eq!(
        space.map(start, size, writable, false, runtime),
        Err(KError::NoMemory)
    );
    assert!(space.find_vma(start).is_none());
    assert_eq!(crate::committed_pages(), baseline);

    // Read-only reservations do not promise private COW backing yet.
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    space
        .map_with_max_flags(
            start,
            size,
            writable - MappingFlags::WRITE,
            writable,
            false,
            runtime,
        )
        .unwrap();
    assert_eq!(space.protect(start, size, writable), Err(KError::NoMemory));
    assert!(
        !space
            .find_vma(start)
            .unwrap()
            .flags()
            .contains(MappingFlags::WRITE)
    );
    space.clear();

    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    let vma = VmArea::new(
        start,
        size,
        writable,
        writable,
        runtime.backing_info(),
        0,
        None,
    )
    .with_noreserve(true);
    space.map_runtime_vma(vma, false, runtime).unwrap();
    assert!(space.pgtbl.query(start).is_err());
    assert_eq!(crate::committed_pages(), baseline);
    space.clear();
    assert_eq!(crate::committed_pages(), baseline);

    // Individually admissible growth may create a VMA larger than RAM;
    // a new fork or DONTUNMAP commitment must still be admitted separately.
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    space
        .map(start, PAGE_SIZE_4K, writable, false, runtime)
        .unwrap();
    space.extend_area(start, size - PAGE_SIZE_4K).unwrap();
    assert_eq!(crate::committed_pages(), baseline + size / PAGE_SIZE_4K);
    assert!(matches!(space.try_clone(), Err(KError::NoMemory)));
    let source = space.resolve_mremap_source(start, size).unwrap();
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    assert!(matches!(
        source.fresh_mapping(size, &runtime),
        Err(KError::NoMemory)
    ));
    assert_eq!(space.find_vma(start).unwrap().size(), size);
    space.clear();
    assert_eq!(crate::committed_pages(), baseline);
}

#[def_test(serial)]
fn private_commitments_follow_protection_splits_fork_and_partial_unmap() {
    let baseline = crate::committed_pages();
    let mut space = new_test_aspace();
    let start = space.base();
    let writable = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    space
        .map_with_max_flags(
            start,
            3 * PAGE_SIZE_4K,
            writable - MappingFlags::WRITE,
            writable,
            false,
            runtime,
        )
        .unwrap();
    space
        .protect(start + PAGE_SIZE_4K, PAGE_SIZE_4K, writable)
        .unwrap();
    assert_eq!(crate::committed_pages(), baseline + 1);
    space
        .protect(
            start + PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            writable - MappingFlags::WRITE,
        )
        .unwrap();
    assert_eq!(crate::committed_pages(), baseline + 1);
    let child = space.try_clone().unwrap();
    assert_eq!(crate::committed_pages(), baseline + 2);
    space.unmap(start + PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
    assert_eq!(crate::committed_pages(), baseline + 1);
    drop(child);
    space.clear();
    assert_eq!(crate::committed_pages(), baseline);
}

#[def_test(serial)]
fn private_commitments_transfer_on_remap_and_rollback_on_destination_conflict() {
    let baseline = crate::committed_pages();
    let owner = Arc::new(Mutex::new(new_test_aspace()));
    let mut space = owner.lock();
    let start = space.base();
    let writable = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER;
    let runtime = VmRuntimeRef::new_anon_private(start, PageSize::Size4K);
    space
        .map(start, 2 * PAGE_SIZE_4K, writable, false, runtime)
        .unwrap();
    let source = space
        .resolve_mremap_source(start, 2 * PAGE_SIZE_4K)
        .unwrap();
    assert_eq!(
        space.relocate_mapping(
            &source,
            start + PAGE_SIZE_4K,
            3 * PAGE_SIZE_4K,
            false,
            &owner
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(crate::committed_pages(), baseline + 2);
    let target = start + 8 * PAGE_SIZE_4K;
    let conflict = VmRuntimeRef::new_anon_private(target, PageSize::Size4K);
    space
        .map(target, PAGE_SIZE_4K, writable, false, conflict)
        .unwrap();
    assert_eq!(
        space.relocate_mapping(&source, target, 3 * PAGE_SIZE_4K, false, &owner),
        Err(KError::AlreadyExists)
    );
    assert_eq!(crate::committed_pages(), baseline + 3);
    space.unmap(target, PAGE_SIZE_4K).unwrap();
    space
        .relocate_mapping(&source, target, 3 * PAGE_SIZE_4K, false, &owner)
        .unwrap();
    assert_eq!(crate::committed_pages(), baseline + 3);
    space.unmap(target + PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
    assert_eq!(crate::committed_pages(), baseline + 2);
    space.clear();
    assert_eq!(crate::committed_pages(), baseline);
}
