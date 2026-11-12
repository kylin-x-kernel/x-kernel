// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise the owned PTE mutation paths, with completion observed by each hold.

use alloc::{alloc::Layout, sync::Arc, vec::Vec};
use core::{
    mem::ManuallyDrop,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
};

use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use unittest::def_test;

use super::tests::TestEntry;
use crate::{
    PageSize, PageTable64, PageTableEntry, PagingFlags, PagingHandler, PagingMetaData, PtError,
    TlbFlushError, TlbFlushResult,
};

struct Lifetime {
    address: usize,
    bytes: usize,
    withdrawn_at: AtomicUsize,
    invalidated_at: AtomicUsize,
    drops: AtomicUsize,
}

struct Trace {
    pages: Vec<Lifetime>,
    sequence: AtomicUsize,
    singles: AtomicUsize,
    fulls: AtomicUsize,
    fences: AtomicUsize,
    fail_next_allocation: AtomicBool,
    fail_next_clone: AtomicBool,
    fail_next_flush: AtomicBool,
    prevent_flush_completion: AtomicBool,
    forbidden_table: AtomicUsize,
}

impl Trace {
    fn withdraw(&self, index: usize) {
        self.pages[index]
            .withdrawn_at
            .store(self.sequence.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn invalidate(&self, address: Option<VirtAddr>) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if address.is_some() {
            self.singles.fetch_add(1, Ordering::Relaxed);
        } else {
            self.fulls.fetch_add(1, Ordering::Relaxed);
        }
        for page in &self.pages {
            if address
                .is_none_or(|address| address.as_usize().wrapping_sub(page.address) < page.bytes)
            {
                page.invalidated_at.store(sequence, Ordering::Relaxed);
            }
        }
    }

    fn assert_counts(&self, singles: usize, fulls: usize) {
        assert_eq!(self.singles.load(Ordering::Relaxed), singles);
        assert_eq!(self.fulls.load(Ordering::Relaxed), fulls);
    }

    fn assert_drops(&self, index: usize, count: usize) {
        assert_eq!(self.pages[index].drops.load(Ordering::Relaxed), count);
    }
}

// Only GatherMeta uses this hook. Each serial test installs a scoped fixture
// before constructing its inactive page table and drops that table first.
static ACTIVE_TRACE: AtomicPtr<Trace> = AtomicPtr::new(core::ptr::null_mut());

struct Fixture(Arc<Trace>);

impl Fixture {
    fn new(layout: impl IntoIterator<Item = (usize, usize)>) -> Self {
        let trace = Arc::new(Trace {
            pages: layout
                .into_iter()
                .map(|(address, bytes)| Lifetime {
                    address,
                    bytes,
                    withdrawn_at: AtomicUsize::new(usize::MAX),
                    invalidated_at: AtomicUsize::new(0),
                    drops: AtomicUsize::new(0),
                })
                .collect(),
            sequence: AtomicUsize::new(0),
            singles: AtomicUsize::new(0),
            fulls: AtomicUsize::new(0),
            fences: AtomicUsize::new(0),
            fail_next_allocation: AtomicBool::new(false),
            fail_next_clone: AtomicBool::new(false),
            fail_next_flush: AtomicBool::new(false),
            prevent_flush_completion: AtomicBool::new(false),
            forbidden_table: AtomicUsize::new(0),
        });
        assert!(
            ACTIVE_TRACE
                .compare_exchange(
                    core::ptr::null_mut(),
                    Arc::as_ptr(&trace).cast_mut(),
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
        );
        Self(trace)
    }

    fn reference(&self, index: usize) -> Reference {
        Reference {
            trace: Some(Arc::clone(&self.0)),
            index,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        ACTIVE_TRACE.store(core::ptr::null_mut(), Ordering::Release);
        assert_eq!(Arc::strong_count(&self.0), 1, "all mapping holds retired");
    }
}

fn with_active_trace(f: impl FnOnce(&Trace)) {
    let ptr = ACTIVE_TRACE.load(Ordering::Acquire);
    assert!(!ptr.is_null());
    // SAFETY: Only the serial tests below invoke GatherMeta. Their Fixture
    // owns this Arc allocation until all table operations and destructors
    // complete. The reference is used only during the synchronous hook call.
    let trace = unsafe { &*ptr };
    f(trace);
}

struct Reference {
    trace: Option<Arc<Trace>>,
    index: usize,
}

impl Drop for Reference {
    fn drop(&mut self) {
        let Some(trace) = &self.trace else {
            return;
        };
        let page = &trace.pages[self.index];
        let withdrawn = page.withdrawn_at.load(Ordering::Relaxed);
        if withdrawn != usize::MAX {
            assert!(
                page.invalidated_at.load(Ordering::Relaxed) > withdrawn,
                "hold ended before its PTE invalidation completed"
            );
        }
        page.drops.fetch_add(1, Ordering::Relaxed);
    }
}

struct GatherHandler;

impl PagingHandler for GatherHandler {
    type MappingReference = Reference;

    fn mapping_reference_paddr(reference: &Reference) -> PhysAddr {
        PhysAddr::from(0x4000_0000 + reference.index * (1 << 30))
    }

    fn mapping_reference_bytes(reference: &Reference) -> usize {
        reference.trace.as_ref().unwrap().pages[reference.index].bytes
    }

    fn mapping_reference_into_raw(mut reference: Reference) -> (PhysAddr, usize) {
        let paddr = Self::mapping_reference_paddr(&reference);
        let bytes = Self::mapping_reference_bytes(&reference);
        let _raw = Arc::into_raw(reference.trace.take().unwrap());
        (paddr, bytes)
    }

    unsafe fn mapping_reference_from_raw(paddr: PhysAddr, bytes: usize) -> Reference {
        let index = (paddr.as_usize() - 0x4000_0000) / (1 << 30);
        let ptr = ACTIVE_TRACE.load(Ordering::Acquire);
        // SAFETY: Every transferred Reference used Arc::into_raw for this
        // fixture. The removed leaf selects exactly one outstanding transfer.
        let trace = unsafe { Arc::from_raw(ptr) };
        assert_eq!(trace.pages[index].bytes, bytes);
        Reference {
            trace: Some(trace),
            index,
        }
    }

    unsafe fn mapping_reference_try_clone_from_raw(
        paddr: PhysAddr,
        bytes: usize,
    ) -> crate::PtResult<Reference> {
        let mut fail = false;
        with_active_trace(|trace| fail = trace.fail_next_clone.swap(false, Ordering::Relaxed));
        if fail {
            return Err(PtError::NoMemory);
        }
        // SAFETY: map_existing retains the exact managed leaf transfer under
        // exclusive access. This borrowed reconstruction never consumes it.
        let borrowed = ManuallyDrop::new(unsafe { Self::mapping_reference_from_raw(paddr, bytes) });
        Ok(Reference {
            trace: borrowed.trace.as_ref().map(Arc::clone),
            index: borrowed.index,
        })
    }

    fn alloc_frame() -> Option<PhysAddr> {
        let mut fail = false;
        with_active_trace(|trace| fail = trace.fail_next_allocation.swap(false, Ordering::Relaxed));
        if fail {
            return None;
        }
        let layout = Layout::from_size_align(PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
        // SAFETY: The nonzero page layout is valid; allocation failure is
        // returned as None. Successful storage is zeroed and 4 KiB aligned.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        (!ptr.is_null()).then_some(PhysAddr::from(ptr as usize))
    }

    fn dealloc_frame(address: PhysAddr) {
        let layout = Layout::from_size_align(PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
        // SAFETY: PageTable64 returns each table from alloc_frame once, after
        // its last software use. These fixtures never activate hardware roots.
        unsafe { alloc::alloc::dealloc(address.as_usize() as *mut u8, layout) };
    }

    fn p2v(address: PhysAddr) -> VirtAddr {
        with_active_trace(|trace| {
            assert_ne!(
                trace.forbidden_table.load(Ordering::Relaxed),
                address.as_usize(),
                "borrower must reject mutation before accessing a source child table"
            );
        });
        VirtAddr::from(address.as_usize())
    }
}

struct GatherMeta;

impl PagingMetaData for GatherMeta {
    type VirtAddr = VirtAddr;

    const INVALID_TO_VALID_NEEDS_TLBI: bool = false;
    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = usize::BITS as usize - 1;
    const VA_MAX_BITS: usize = usize::BITS as usize - 1;

    fn vaddr_is_valid(_address: usize) -> bool {
        true
    }

    fn flush_tlb(address: Option<VirtAddr>) {
        with_active_trace(|trace| trace.invalidate(address));
    }

    fn flush_tlb_process(address: Option<VirtAddr>) -> TlbFlushResult {
        Self::flush_tlb(address);
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn flush_tlb_process_mask(
        address: Option<VirtAddr>,
        _mask: kcpu_id_map::KCpuMask,
    ) -> TlbFlushResult {
        Self::flush_tlb_process(address)
    }

    fn flush_tlb_all_cpus(address: Option<VirtAddr>) -> TlbFlushResult {
        let mut completed = false;
        with_active_trace(|trace| {
            if !trace.prevent_flush_completion.load(Ordering::Relaxed)
                && !trace.fail_next_flush.swap(false, Ordering::Relaxed)
            {
                trace.invalidate(address);
                completed = true;
            }
        });
        if completed {
            Ok(())
        } else {
            Err(TlbFlushError::TimedOut)
        }
    }

    fn fence_fresh_map() {
        with_active_trace(|trace| {
            trace.fences.fetch_add(1, Ordering::Relaxed);
        });
    }
}

type Table = PageTable64<GatherMeta, TestEntry, GatherHandler>;

#[def_test(serial)]
fn owned_unmap_aggregates_across_retirement_boundaries() {
    for count in [1, 16, 17, 32, 33, 255, 256, 257, 1024] {
        let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
        let mut table = Table::try_new_kernel().unwrap();
        let mut edit = table.modify();
        for index in 0..count {
            edit.map(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
        }
        assert!(!edit.finish().had_pending());
        assert_eq!(fixture.0.fences.load(Ordering::Relaxed), 1);
        for index in 0..count {
            fixture.0.withdraw(index);
            edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
        }
        edit.finish();
        let tail = count % 256;
        fixture.0.assert_counts(
            if tail <= 16 { tail } else { 0 },
            count / 256 + usize::from(tail > 16),
        );
        for index in 0..count {
            fixture.0.assert_drops(index, 1);
        }
        assert!(!edit.finish().had_pending());
        drop(edit);
        drop(table);
    }
}

#[def_test(serial)]
fn sparse_owned_and_raw_unmap_share_one_invalidation_summary() {
    let count = 33;
    let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..count {
        let address = VirtAddr::from(index * PAGE_SIZE_4K);
        if index % 2 == 0 {
            edit.map(address, fixture.reference(index), PagingFlags::READ)
                .unwrap();
            fixture.0.withdraw(index);
        } else {
            edit.map_raw(
                address,
                PhysAddr::from(0x8000_0000),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .unwrap();
        }
    }
    edit.finish();
    edit.unmap_sparse_region(VirtAddr::from(0), count * PAGE_SIZE_4K, PageSize::Size4K)
        .unwrap();
    edit.finish();
    fixture.0.assert_counts(0, 1);
    for index in 0..count {
        fixture.0.assert_drops(index, usize::from(index % 2 == 0));
    }
}

#[def_test(serial)]
fn unmap_error_finishes_only_the_successful_owned_prefix() {
    let fixture = Fixture::new((0..33).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    {
        let mut edit = table.modify();
        for index in 0..33 {
            if index == 32 {
                continue;
            }
            edit.map(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
            fixture.0.withdraw(index);
        }
        edit.finish();
        assert_eq!(
            edit.unmap_region(VirtAddr::from(0), 33 * PAGE_SIZE_4K),
            Err(PtError::NotMapped)
        );
        for index in 0..32 {
            fixture.0.assert_drops(index, 0);
        }
    }
    fixture.0.assert_counts(0, 1);
    for index in 0..32 {
        fixture.0.assert_drops(index, 1);
    }
    fixture.0.assert_drops(32, 0);
}

#[def_test(serial)]
fn retirement_reservation_failure_makes_progress_without_carrying_a_hold() {
    for count in [1, 16, 17, 32, 33, 1024] {
        let fixture = Fixture::new((0..count).map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K)));
        let mut table = Table::try_new_kernel().unwrap();
        let mut edit = table.modify();
        edit.retired_mapping_refs.should_fail_reservation = true;
        for index in 0..count {
            edit.map(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
        }
        edit.finish();
        for index in 0..count {
            fixture.0.withdraw(index);
            edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
        }
        edit.finish();
        fixture.0.assert_counts(count % 17, count / 17);
        for index in 0..count {
            fixture.0.assert_drops(index, 1);
        }
    }
}

#[def_test(serial)]
fn owned_replacement_deduplicates_one_address_with_many_retired_holds() {
    let fixture = Fixture::new((0..257).map(|_| (0, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let address = VirtAddr::from(0);
    edit.map(address, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    for index in 1..257 {
        let snapshot = edit.query_entry(address).unwrap();
        fixture.0.withdraw(index - 1);
        edit.replace_if_same(
            address,
            snapshot,
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    fixture.0.assert_counts(1, 0);
    for index in 0..256 {
        fixture.0.assert_drops(index, 1);
    }
    fixture.0.assert_drops(256, 0);
    assert!(!edit.finish().had_pending());
    fixture.0.withdraw(256);
    edit.protect(address, PagingFlags::empty()).unwrap();
    fixture.0.assert_drops(256, 0);
    drop(edit);
    fixture.0.assert_counts(2, 0);
    fixture.0.assert_drops(256, 1);
}

#[def_test(serial)]
fn sparse_owned_error_keeps_huge_leaf_and_retires_the_prefix() {
    let fixture = Fixture::new(
        (0..33)
            .map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K))
            .chain(core::iter::once((1 << 21, 1 << 21))),
    );
    let mut table = Table::try_new_kernel().unwrap();
    {
        let mut edit = table.modify();
        for index in 0..33 {
            edit.map(
                VirtAddr::from(index * PAGE_SIZE_4K),
                fixture.reference(index),
                PagingFlags::READ,
            )
            .unwrap();
            fixture.0.withdraw(index);
        }
        edit.map_sized(
            VirtAddr::from(1 << 21),
            fixture.reference(33),
            PageSize::Size2M,
            PagingFlags::READ,
        )
        .unwrap();
        edit.finish();
        assert_eq!(
            edit.unmap_sparse_region(VirtAddr::from(0), 2 << 21, PageSize::Size4K),
            Err(PtError::MappedToHugePage),
        );
        assert!(edit.query(VirtAddr::from(1 << 21)).is_ok());
        fixture.0.assert_drops(33, 0);
    }
    fixture.0.assert_counts(0, 1);
    for index in 0..33 {
        fixture.0.assert_drops(index, 1);
    }
    {
        let mut edit = table.modify();
        fixture.0.withdraw(33);
        edit.unmap(VirtAddr::from((1 << 21) + PAGE_SIZE_4K))
            .unwrap();
        fixture.0.assert_drops(33, 0);
    }
    fixture.0.assert_counts(0, 2);
    fixture.0.assert_drops(33, 1);
}

#[def_test(serial)]
fn sparse_addresses_and_top_page_retire_without_overflow() {
    let last_page = usize::MAX & !(PAGE_SIZE_4K - 1);
    let fixture = Fixture::new([
        (last_page - 2 * PAGE_SIZE_4K, PAGE_SIZE_4K),
        (last_page, PAGE_SIZE_4K),
    ]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    // Distinct invalidation addresses remain representable at the top page;
    // recording them must not construct an overflowing exclusive range end.
    for offset in [0, 2] {
        edit.map_raw(
            VirtAddr::from(last_page - offset * PAGE_SIZE_4K),
            PhysAddr::from(0x4000_0000),
            PageSize::Size4K,
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    for offset in [0, 2] {
        edit.unmap(VirtAddr::from(last_page - offset * PAGE_SIZE_4K))
            .unwrap();
    }
    edit.finish();
    fixture.0.assert_counts(2, 0);
}

#[def_test(serial)]
fn empty_owned_replacement_at_drain_retires_only_the_old_mapping() {
    let fixture = Fixture::new(
        (0..256)
            .map(|index| (index * PAGE_SIZE_4K, PAGE_SIZE_4K))
            .chain(core::iter::once((255 * PAGE_SIZE_4K, PAGE_SIZE_4K))),
    );
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..256 {
        edit.map(
            VirtAddr::from(index * PAGE_SIZE_4K),
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    for index in 0..255 {
        fixture.0.withdraw(index);
        edit.unmap(VirtAddr::from(index * PAGE_SIZE_4K)).unwrap();
    }
    let address = VirtAddr::from(255 * PAGE_SIZE_4K);
    let snapshot = edit.query_entry(address).unwrap();
    fixture.0.withdraw(255);
    // The incoming hold was never published and has no TLB retirement duty.
    // Only the existing mapped hold must survive the pending invalidation.
    edit.replace_if_same(
        address,
        snapshot,
        fixture.reference(256),
        PagingFlags::empty(),
    )
    .unwrap();
    fixture.0.assert_counts(0, 1);
    for index in 0..257 {
        fixture.0.assert_drops(index, 1);
    }
    assert!(!edit.finish().had_pending());
}

#[def_test(serial)]
fn owned_permission_clears_and_failed_compare_preserve_lifetime_order() {
    let fixture = Fixture::new((0..34).map(|index| ((index % 33) * PAGE_SIZE_4K, PAGE_SIZE_4K)));
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for index in 0..33 {
        edit.map(
            VirtAddr::from(index * PAGE_SIZE_4K),
            fixture.reference(index),
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    let address = VirtAddr::from(0);
    let stale = edit.query_entry(address).unwrap();
    edit.protect(address, PagingFlags::READ | PagingFlags::WRITE)
        .unwrap();
    assert!(matches!(
        edit.replace_if_same(address, stale, fixture.reference(33), PagingFlags::READ),
        Err(crate::PteReplaceError::Changed { .. })
    ));
    fixture.0.assert_drops(0, 0);
    fixture.0.assert_drops(33, 1);
    for index in 0..33 {
        let address = VirtAddr::from(index * PAGE_SIZE_4K);
        fixture.0.withdraw(index);
        match index % 3 {
            0 => {
                edit.protect(address, PagingFlags::empty()).unwrap();
            }
            1 => {
                let (physical, ..) = edit.query(address).unwrap();
                edit.remap_raw(address, physical, PagingFlags::empty())
                    .unwrap();
            }
            _ => {
                let snapshot = edit.query_entry(address).unwrap();
                edit.replace_if_same_raw(address, snapshot, snapshot.paddr(), PagingFlags::empty())
                    .unwrap();
            }
        }
        fixture.0.assert_drops(index, 0);
    }
    drop(edit);
    fixture.0.assert_counts(0, 1);
    for index in 0..34 {
        fixture.0.assert_drops(index, 1);
    }
}

#[def_test(serial)]
fn owned_huge_leaves_keep_whole_capabilities_until_full_completion() {
    let fixture = Fixture::new([(1 << 21, 1 << 21), (1 << 30, 1 << 30)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for (index, size) in [PageSize::Size2M, PageSize::Size1G].into_iter().enumerate() {
        edit.map_sized(
            VirtAddr::from(size as usize),
            fixture.reference(index),
            size,
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from((1 << 21) + 19 * PAGE_SIZE_4K))
        .unwrap();
    fixture.0.withdraw(1);
    edit.protect(
        VirtAddr::from((1 << 30) + 23 * PAGE_SIZE_4K),
        PagingFlags::empty(),
    )
    .unwrap();
    for index in 0..2 {
        fixture.0.assert_drops(index, 0);
    }
    drop(edit);
    fixture.0.assert_counts(0, 1);
    for index in 0..2 {
        fixture.0.assert_drops(index, 1);
    }
}

#[def_test(serial)]
fn failed_map_releases_only_the_supplied_hold_and_preserves_existing_mapping() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K), (0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let original = edit.query_entry(VirtAddr::from(0)).unwrap();
    assert_eq!(
        edit.map(VirtAddr::from(0), fixture.reference(1), PagingFlags::READ),
        Err(PtError::AlreadyMapped)
    );
    assert_eq!(edit.query_entry(VirtAddr::from(0)).unwrap(), original);
    fixture.0.assert_drops(0, 0);
    fixture.0.assert_drops(1, 1);
    drop(edit);
    drop(table);
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn table_allocation_failure_rolls_back_the_unpublished_mapping_hold() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    fixture
        .0
        .fail_next_allocation
        .store(true, Ordering::Relaxed);
    let mut edit = table.modify();
    assert_eq!(
        edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ),
        Err(PtError::NoMemory)
    );
    assert_eq!(edit.query(VirtAddr::from(0)), Err(PtError::NotMapped));
    fixture.0.assert_drops(0, 1);
    assert!(!edit.finish().had_pending());
}

#[def_test(serial)]
fn raw_replacement_cannot_detach_a_managed_leaf_from_its_hold() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let va = VirtAddr::from(0);
    edit.map(va, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let expected = edit.query_entry(va).unwrap();
    let other = PhysAddr::from(0x8000_0000);
    assert_eq!(
        edit.remap_raw(va, other, PagingFlags::READ),
        Err(PtError::InvalidAddress)
    );
    assert_eq!(
        edit.replace_if_same_raw(va, expected, other, PagingFlags::READ),
        Err(crate::PteReplaceError::PageTable(PtError::InvalidAddress))
    );
    assert_eq!(edit.query_entry(va).unwrap(), expected);
    fixture.0.assert_drops(0, 0);
    assert!(!edit.finish().had_pending());
    edit.remap_raw(va, expected.paddr(), PagingFlags::READ | PagingFlags::WRITE)
        .unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 0);
}

#[def_test(serial)]
fn huge_raw_permission_updates_normalize_the_pa_and_find_the_containing_hold() {
    let fixture = Fixture::new([(1 << 21, 1 << 21)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let base = VirtAddr::from(1 << 21);
    let interior = VirtAddr::from((1 << 21) + 3 * PAGE_SIZE_4K);
    edit.map_sized(
        base,
        fixture.reference(0),
        PageSize::Size2M,
        PagingFlags::READ,
    )
    .unwrap();
    edit.finish();
    let expected = edit.query_entry(interior).unwrap();
    edit.remap_raw(
        interior,
        expected.paddr() + 3 * PAGE_SIZE_4K,
        PagingFlags::READ | PagingFlags::WRITE,
    )
    .unwrap();
    assert_eq!(edit.query_entry(base).unwrap().paddr(), expected.paddr());
    edit.finish();
    fixture.0.assert_drops(0, 0);
    fixture.0.withdraw(0);
    let snapshot = edit.query_entry(interior).unwrap();
    edit.replace_if_same_raw(
        interior,
        snapshot,
        snapshot.paddr() + PAGE_SIZE_4K,
        PagingFlags::empty(),
    )
    .unwrap();
    fixture.0.assert_drops(0, 0);
    edit.finish();
    fixture.0.assert_drops(0, 1);
}

#[cfg(feature = "copy-from")]
#[def_test(serial)]
fn borrowed_subtrees_keep_source_holds_and_refuse_borrower_mutation() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K), (PAGE_SIZE_4K, PAGE_SIZE_4K)]);
    let mut source = Table::try_new_kernel().unwrap();
    let mut target = Table::try_new().unwrap();
    source
        .modify()
        .map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    let source_child = source.table_of(source.root_paddr)[0].paddr();
    fixture
        .0
        .forbidden_table
        .store(source_child.as_usize(), Ordering::Relaxed);
    {
        let mut edit = target.modify();
        edit.copy_from(&source, VirtAddr::from(0), PAGE_SIZE_4K);
        assert_eq!(edit.unmap(VirtAddr::from(0)), Err(PtError::InvalidAddress));
        assert_eq!(
            edit.map_existing(VirtAddr::from(0), VirtAddr::from(PAGE_SIZE_4K)),
            Err(PtError::InvalidAddress)
        );
        assert_eq!(
            edit.map(
                VirtAddr::from(PAGE_SIZE_4K),
                fixture.reference(1),
                PagingFlags::READ
            ),
            Err(PtError::InvalidAddress)
        );
        assert_eq!(
            edit.unmap_sparse_region(VirtAddr::from(0), PAGE_SIZE_4K, PageSize::Size4K),
            Err(PtError::InvalidAddress)
        );
    }
    fixture.0.assert_drops(0, 0);
    fixture.0.assert_drops(1, 1);
    drop(target);
    fixture.0.assert_drops(0, 0);
    fixture.0.forbidden_table.store(0, Ordering::Relaxed);
    assert!(source.query(VirtAddr::from(0)).is_ok());
    drop(source);
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn unconfirmed_shootdown_retains_data_until_a_successful_full_retry() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.try_finish().unwrap();
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    fixture.0.fail_next_flush.store(true, Ordering::Relaxed);
    assert_eq!(edit.try_finish(), Err(TlbFlushError::TimedOut));
    fixture.0.assert_drops(0, 0);
    assert!(matches!(edit.flush, super::ToFlush::Full));
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
    fixture.0.assert_counts(0, 1);
}

#[def_test(serial)]
fn empty_permissions_and_invalid_capabilities_install_no_owned_leaf() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K), (0, 3 * PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(
        VirtAddr::from(0),
        fixture.reference(0),
        PagingFlags::empty(),
    )
    .unwrap();
    fixture.0.assert_drops(0, 1);
    assert_eq!(
        edit.map(VirtAddr::from(0), fixture.reference(1), PagingFlags::READ),
        Err(PtError::NotAligned)
    );
    fixture.0.assert_drops(1, 1);
    assert_eq!(edit.query(VirtAddr::from(0)), Err(PtError::NotMapped));
    assert!(!edit.finish().had_pending());
}

#[def_test(serial)]
fn guard_drop_does_not_release_a_hold_after_unconfirmed_invalidation() {
    // This synthetic capability contains no heap allocation or actual backing;
    // forgetting it can test destructor suppression without leaking test RAM.
    static RELEASES: AtomicUsize = AtomicUsize::new(0);
    struct Hold;
    impl Drop for Hold {
        fn drop(&mut self) {
            RELEASES.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Handler;
    impl PagingHandler for Handler {
        type MappingReference = Hold;

        fn mapping_reference_paddr(_reference: &Hold) -> PhysAddr {
            PhysAddr::from(0x4000_0000)
        }

        fn mapping_reference_bytes(_reference: &Hold) -> usize {
            PAGE_SIZE_4K
        }

        fn mapping_reference_into_raw(reference: Hold) -> (PhysAddr, usize) {
            core::mem::forget(reference);
            (PhysAddr::from(0x4000_0000), PAGE_SIZE_4K)
        }

        unsafe fn mapping_reference_from_raw(paddr: PhysAddr, bytes: usize) -> Hold {
            assert_eq!(paddr, PhysAddr::from(0x4000_0000));
            assert_eq!(bytes, PAGE_SIZE_4K);
            Hold
        }

        fn alloc_frame() -> Option<PhysAddr> {
            GatherHandler::alloc_frame()
        }

        fn dealloc_frame(address: PhysAddr) {
            GatherHandler::dealloc_frame(address);
        }

        fn p2v(address: PhysAddr) -> VirtAddr {
            GatherHandler::p2v(address)
        }
    }
    RELEASES.store(0, Ordering::Relaxed);
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = PageTable64::<GatherMeta, TestEntry, Handler>::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), Hold, PagingFlags::READ)
        .unwrap();
    edit.try_finish().unwrap();
    // This provider deliberately keeps the default unsupported clone hook.
    assert_eq!(
        edit.map_existing(VirtAddr::from(0), VirtAddr::from(PAGE_SIZE_4K)),
        Err(PtError::InvalidAddress)
    );
    assert!(edit.query(VirtAddr::from(0)).is_ok());
    assert_eq!(RELEASES.load(Ordering::Relaxed), 0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    fixture
        .0
        .prevent_flush_completion
        .store(true, Ordering::Relaxed);
    drop(edit);
    assert_eq!(RELEASES.load(Ordering::Relaxed), 0);
    drop(table);
    assert_eq!(RELEASES.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn automatic_finish_keeps_ownership_without_a_caller_completion_check() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    fixture.0.fail_next_flush.store(true, Ordering::Relaxed);
    // Automatic callers can ignore the diagnostic receipt. The guard, rather
    // than the caller, must retain ownership and the full retry after failure.
    edit.finish();
    fixture.0.assert_drops(0, 0);
    assert!(matches!(edit.flush, super::ToFlush::Full));
    edit.finish();
    fixture.0.assert_drops(0, 1);
    assert!(!edit.finish().had_pending());
}

#[def_test(serial)]
fn synchronous_permission_revocation_reports_failure_without_a_retired_hold() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(
        VirtAddr::from(0),
        fixture.reference(0),
        PagingFlags::READ | PagingFlags::WRITE,
    )
    .unwrap();
    edit.try_finish().unwrap();
    edit.protect(VirtAddr::from(0), PagingFlags::READ).unwrap();
    fixture.0.fail_next_flush.store(true, Ordering::Relaxed);
    assert_eq!(edit.try_finish(), Err(TlbFlushError::TimedOut));
    fixture.0.assert_drops(0, 0);
    assert!(edit.try_finish().unwrap().had_pending());
    assert_eq!(
        edit.query_entry(VirtAddr::from(0)).unwrap().flags(),
        PagingFlags::READ
    );
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn owned_leaf_transfers_without_acquiring_an_extra_reference() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    assert_eq!(Arc::strong_count(&fixture.0), 2);
    assert!(edit.get_entry(VirtAddr::from(0)).unwrap().0.is_managed());
    fixture.0.assert_drops(0, 0);
    edit.finish();
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    assert_eq!(Arc::strong_count(&fixture.0), 2);
    fixture.0.assert_drops(0, 0);
    edit.try_finish().unwrap();
    assert_eq!(Arc::strong_count(&fixture.0), 1);
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn owned_aliases_retire_their_own_transfers_independently() {
    let size = PageSize::Size2M;
    let fixture = Fixture::new([(0, size as usize)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    for address in [0, size as usize] {
        edit.map_sized(
            VirtAddr::from(address),
            fixture.reference(0),
            size,
            PagingFlags::READ,
        )
        .unwrap();
    }
    edit.finish();
    assert_eq!(Arc::strong_count(&fixture.0), 3);
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    fixture.0.assert_drops(0, 0);
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
    assert_eq!(Arc::strong_count(&fixture.0), 2);
    assert!(edit.query(VirtAddr::from(size as usize)).is_ok());
    edit.unmap(VirtAddr::from(size as usize)).unwrap();
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 2);
}

#[def_test(serial)]
fn raw_alias_does_not_acquire_or_release_a_managed_transfer() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    edit.map(VirtAddr::from(0), fixture.reference(0), PagingFlags::READ)
        .unwrap();
    let paddr = edit.query(VirtAddr::from(0)).unwrap().0;
    edit.map_raw(
        VirtAddr::from(PAGE_SIZE_4K),
        paddr,
        PageSize::Size4K,
        PagingFlags::READ,
    )
    .unwrap();
    edit.finish();
    assert!(
        !edit
            .get_entry(VirtAddr::from(PAGE_SIZE_4K))
            .unwrap()
            .0
            .is_managed()
    );
    edit.unmap(VirtAddr::from(PAGE_SIZE_4K)).unwrap();
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 0);
    assert_eq!(Arc::strong_count(&fixture.0), 2);
    fixture.0.withdraw(0);
    edit.unmap(VirtAddr::from(0)).unwrap();
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn inactive_tree_destruction_recovers_live_base_and_huge_leaf_transfers() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K), (1 << 21, 1 << 21), (1 << 30, 1 << 30)]);
    let mut table = Table::try_new_kernel().unwrap();
    {
        let mut edit = table.modify();
        for (index, size) in [PageSize::Size4K, PageSize::Size2M, PageSize::Size1G]
            .into_iter()
            .enumerate()
        {
            edit.map_sized(
                VirtAddr::from(fixture.0.pages[index].address),
                fixture.reference(index),
                size,
                PagingFlags::READ,
            )
            .unwrap();
        }
        edit.finish();
    }
    assert_eq!(Arc::strong_count(&fixture.0), 4);
    // No hardware ever used this tree, so there are no translations to retire.
    drop(table);
    for index in 0..3 {
        fixture.0.assert_drops(index, 1);
    }
}

#[def_test(serial)]
fn empty_replacement_releases_unpublished_input_but_retains_the_old_mapping_on_failure() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K), (0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let address = VirtAddr::from(0);
    edit.map(address, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let snapshot = edit.query_entry(address).unwrap();
    fixture.0.withdraw(0);
    edit.replace_if_same(
        address,
        snapshot,
        fixture.reference(1),
        PagingFlags::empty(),
    )
    .unwrap();
    fixture.0.assert_drops(1, 1);
    fixture.0.assert_drops(0, 0);
    fixture.0.fail_next_flush.store(true, Ordering::Relaxed);
    assert_eq!(edit.try_finish(), Err(TlbFlushError::TimedOut));
    fixture.0.assert_drops(0, 0);
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn three_level_tree_destruction_recovers_a_root_huge_leaf() {
    struct ThreeLevelMeta;
    impl PagingMetaData for ThreeLevelMeta {
        type VirtAddr = VirtAddr;

        const INVALID_TO_VALID_NEEDS_TLBI: bool = false;
        const LEVELS: usize = 3;
        const PA_MAX_BITS: usize = GatherMeta::PA_MAX_BITS;
        const VA_MAX_BITS: usize = 39;

        fn flush_tlb(address: Option<VirtAddr>) {
            GatherMeta::flush_tlb(address);
        }

        fn flush_tlb_all_cpus(address: Option<VirtAddr>) -> TlbFlushResult {
            GatherMeta::flush_tlb_all_cpus(address)
        }
    }
    let fixture = Fixture::new([(0, PageSize::Size1G as usize)]);
    let mut table =
        PageTable64::<ThreeLevelMeta, TestEntry, GatherHandler>::try_new_kernel().unwrap();
    table
        .modify()
        .map_sized(
            VirtAddr::from(0),
            fixture.reference(0),
            PageSize::Size1G,
            PagingFlags::READ,
        )
        .unwrap();
    assert_eq!(Arc::strong_count(&fixture.0), 2);
    // The root itself contains this owned leaf; no child table may be assumed.
    drop(table);
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn map_existing_alias_owns_an_independent_managed_hold() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let source = VirtAddr::from(0);
    let alias = VirtAddr::from(PAGE_SIZE_4K);
    edit.map(source, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    assert_eq!(edit.map_existing(source, alias), Ok(PageSize::Size4K));
    assert_eq!(edit.query(source), edit.query(alias));
    assert_eq!(Arc::strong_count(&fixture.0), 3);
    assert_eq!(
        edit.remap_raw(alias, PhysAddr::from(0x8000_0000), PagingFlags::READ),
        Err(PtError::InvalidAddress)
    );
    edit.unmap(source).unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 1);
    assert!(edit.query(alias).is_ok());
    edit.unmap(alias).unwrap();
    fixture.0.assert_drops(0, 1);
    edit.finish();
    fixture.0.assert_drops(0, 2);
}

#[def_test(serial)]
fn map_existing_rejections_preserve_source_and_destination() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let source = VirtAddr::from(0);
    let alias = VirtAddr::from(PAGE_SIZE_4K);
    edit.map(source, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let original = edit.query_entry(source).unwrap();
    assert_eq!(
        edit.map_existing(source, source),
        Err(PtError::AlreadyMapped)
    );
    assert_eq!(
        edit.map_existing(source, VirtAddr::from(1)),
        Err(PtError::NotAligned)
    );
    fixture.0.fail_next_clone.store(true, Ordering::Relaxed);
    assert_eq!(edit.map_existing(source, alias), Err(PtError::NoMemory));
    assert_eq!(edit.query_entry(source).unwrap(), original);
    assert_eq!(edit.query(alias), Err(PtError::NotMapped));
    fixture.0.assert_drops(0, 0);
}

#[def_test(serial)]
fn map_existing_preparation_failure_rolls_back_only_destination_prefix() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let source = VirtAddr::from(0);
    let first = VirtAddr::from(PAGE_SIZE_4K);
    let second = VirtAddr::from(1 << 30);
    edit.map(source, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let original = edit.query_entry(source).unwrap();
    edit.map_existing(source, first).unwrap();
    fixture
        .0
        .fail_next_allocation
        .store(true, Ordering::Relaxed);
    assert_eq!(edit.map_existing(source, second), Err(PtError::NoMemory));
    fixture.0.assert_drops(0, 1); // Only the unpublished clone ended.
    assert_eq!(edit.query_entry(source).unwrap(), original);
    assert_eq!(edit.query(second), Err(PtError::NotMapped));
    assert!(edit.query(first).is_ok());
    edit.unmap(first).unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 2);
    assert_eq!(edit.query_entry(source).unwrap(), original);
}

#[def_test(serial)]
fn map_existing_raw_leaf_does_not_acquire_a_managed_hold() {
    let fixture = Fixture::new([]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let source = VirtAddr::from(0);
    let target = VirtAddr::from(PAGE_SIZE_4K);
    edit.map_raw(
        source,
        PhysAddr::from(0x4000_0000),
        PageSize::Size4K,
        PagingFlags::READ,
    )
    .unwrap();
    fixture.0.fail_next_clone.store(true, Ordering::Relaxed);
    edit.map_existing(source, target).unwrap();
    assert!(fixture.0.fail_next_clone.load(Ordering::Relaxed));
    edit.remap_raw(target, PhysAddr::from(0x8000_0000), PagingFlags::READ)
        .unwrap();
    assert_eq!(Arc::strong_count(&fixture.0), 1);
}

#[def_test(serial)]
fn external_replacement_commits_raw_target_and_retires_only_source() {
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let address = VirtAddr::from(0);
    edit.map(address, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let expected = edit.query_entry(address).unwrap();
    let target = PhysAddr::from(0x8000_0000);
    // SAFETY: The synthetic raw target has no reclaiming owner or hardware
    // users. It stays externally valid for the entire inactive fixture.
    let result = unsafe {
        edit.replace_managed_with_external_if_same(
            address,
            expected,
            target,
            PagingFlags::READ | PagingFlags::WRITE,
        )
    };
    assert_eq!(result, Ok(expected));
    assert_eq!(edit.query(address).unwrap().0, target);
    fixture.0.assert_drops(0, 0);
    fixture.0.fail_next_flush.store(true, Ordering::Relaxed);
    assert!(edit.finish().had_pending());
    fixture.0.assert_drops(0, 0);
    assert_eq!(edit.query(address).unwrap().0, target);
    edit.try_finish().unwrap();
    fixture.0.assert_drops(0, 1);
    // An address change is allowed now: the replacement is raw, not managed.
    edit.remap_raw(address, PhysAddr::from(0xc000_0000), PagingFlags::READ)
        .unwrap();
    edit.unmap(address).unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 1);
}

#[def_test(serial)]
fn external_replacement_rejects_invalid_or_stale_inputs_without_mutation() {
    use crate::PteReplaceError;
    let fixture = Fixture::new([(0, PAGE_SIZE_4K)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let address = VirtAddr::from(0);
    let target = PhysAddr::from(0x8000_0000);
    edit.map(address, fixture.reference(0), PagingFlags::READ)
        .unwrap();
    edit.finish();
    let stale = edit.query_entry(address).unwrap();
    for (va, pa, flags, error) in [
        (address, target, PagingFlags::USER, PtError::InvalidAddress),
        (address, target + 1, PagingFlags::READ, PtError::NotAligned),
        (
            VirtAddr::from(1),
            target,
            PagingFlags::READ,
            PtError::NotAligned,
        ),
    ] {
        // SAFETY: the retained synthetic target is never accessed; invalid
        // inputs must fail before publishing or consuming either owner.
        let result = unsafe { edit.replace_managed_with_external_if_same(va, stale, pa, flags) };
        assert_eq!(result, Err(PteReplaceError::PageTable(error)));
        assert_eq!(edit.query_entry(address).unwrap(), stale);
    }
    edit.protect(address, PagingFlags::READ | PagingFlags::USER)
        .unwrap();
    let current = edit.query_entry(address).unwrap();
    // SAFETY: the retained synthetic target stays valid; stale comparison
    // cannot publish it or consume the live source transfer.
    let result = unsafe {
        edit.replace_managed_with_external_if_same(address, stale, target, PagingFlags::READ)
    };
    assert_eq!(
        result,
        Err(PteReplaceError::Changed {
            current: Some(current)
        })
    );
    assert_eq!(edit.query_entry(address).unwrap(), current);
    fixture.0.assert_drops(0, 0);
    edit.unmap(address).unwrap();
    edit.finish();
    // SAFETY: the retained synthetic target is not published for an absent leaf.
    let result = unsafe {
        edit.replace_managed_with_external_if_same(address, current, target, PagingFlags::READ)
    };
    assert_eq!(result, Err(PteReplaceError::Changed { current: None }));
    edit.map_raw(address, target, PageSize::Size4K, PagingFlags::READ)
        .unwrap();
    let raw = edit.query_entry(address).unwrap();
    // SAFETY: the retained synthetic target is never consumed; the raw source
    // must be rejected without changing its external ownership.
    let result = unsafe {
        edit.replace_managed_with_external_if_same(address, raw, target, PagingFlags::READ)
    };
    assert_eq!(
        result,
        Err(PteReplaceError::PageTable(PtError::InvalidAddress))
    );
}

#[def_test(serial)]
fn map_existing_huge_leaf_requires_exact_bases_and_preserves_geometry() {
    let bytes = PageSize::Size2M as usize;
    let fixture = Fixture::new([(0, bytes)]);
    let mut table = Table::try_new_kernel().unwrap();
    let mut edit = table.modify();
    let source = VirtAddr::from(0);
    let alias = VirtAddr::from(bytes);
    edit.map_sized(
        source,
        fixture.reference(0),
        PageSize::Size2M,
        PagingFlags::READ,
    )
    .unwrap();
    edit.finish();
    assert_eq!(
        edit.map_existing(VirtAddr::from(PAGE_SIZE_4K), alias),
        Err(PtError::NotAligned)
    );
    assert_eq!(
        edit.map_existing(source, VirtAddr::from(bytes + PAGE_SIZE_4K)),
        Err(PtError::NotAligned)
    );
    assert_eq!(edit.map_existing(source, alias), Ok(PageSize::Size2M));
    assert_eq!(
        edit.query(alias + PAGE_SIZE_4K).unwrap().2,
        PageSize::Size2M
    );
    edit.unmap(source).unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 1);
    assert!(edit.query(alias).is_ok());
    edit.unmap(alias).unwrap();
    edit.finish();
    fixture.0.assert_drops(0, 2);
}
