// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::Ordering;

use kspin::NoPreemptIrqSave;
use page_meta::ReferenceState;
use unittest::def_test;

use super::*;

fn counters(kind: UsageKind) -> (usize, usize) {
    crate::with_current_usage_counters(|counter| {
        (
            counter.allocated_bytes[kind as usize].load(Ordering::Relaxed),
            counter.freed_bytes[kind as usize].load(Ordering::Relaxed),
        )
    })
}

fn count(page: &PageRef, index: usize) -> ReferenceState {
    global_meta(page.pfn() + index).reference_state()
}

#[def_test]
fn exact_range_clones_only_selected_pages_and_releases_the_rest() {
    let _guard = NoPreemptIrqSave::new();
    let kind = UsageKind::VirtMem;
    let before = counters(kind);
    let owner = alloc_pages_exact_ref(3, PAGE_SIZE_4K, kind).unwrap();
    let base = owner.phys_addr();
    assert_eq!(owner.page_count(), 3);
    assert_eq!(
        owner.retained_range(),
        PhysAddrRange::from_start_size(base, 3 * PAGE_SIZE_4K)
    );
    // Raw inventory charged four pages; exact admission returned the spare.
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + PAGE_SIZE_4K)
    );
    for index in 0..3 {
        assert_eq!(
            count(&owner, index),
            ReferenceState::Referenced { references: 1 }
        );
    }
    let whole = owner.try_clone_ref().unwrap();
    let last = owner.try_clone_page(2).unwrap();
    assert_eq!(last.page_count(), 1);
    assert_eq!(
        last.retained_range(),
        PhysAddrRange::from_start_size(last.phys_addr(), PAGE_SIZE_4K)
    );
    assert_eq!(
        count(&owner, 0),
        ReferenceState::Referenced { references: 2 }
    );
    assert_eq!(
        count(&owner, 2),
        ReferenceState::Referenced { references: 3 }
    );
    let raw = owner.into_raw();
    assert_eq!(raw, (base, 3));
    // SAFETY: restore the exact original range transfer once; whole and last
    // retain additional valid references and no content/hardware use exists.
    let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
    drop(restored);
    drop(whole);
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + 3 * PAGE_SIZE_4K)
    );
    assert_eq!(
        count(&last, 0),
        ReferenceState::Referenced { references: 1 }
    );
    drop(last);
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + 4 * PAGE_SIZE_4K)
    );
}

#[def_test]
fn exact_subranges_with_the_same_start_have_independent_lengths() {
    let owner = alloc_pages_exact_ref(6, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    let short = owner.try_clone_range(0, 2).unwrap();
    let nested = owner.try_clone_range(2, 3).unwrap();
    let nested_last = nested.try_clone_page(2).unwrap();
    assert_eq!(short.phys_addr(), owner.phys_addr());
    assert_eq!(short.page_count(), 2);
    assert_eq!(
        nested_last.phys_addr().as_usize(),
        owner.phys_addr().as_usize() + 4 * PAGE_SIZE_4K
    );
    assert_eq!(nested_last.page_count(), 1);
    let raw = short.into_raw();
    drop(owner);
    // SAFETY: consume this exact two-page transfer, independent of the original
    // six-page length; these two holds survived the original owner's Drop.
    let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
    assert_eq!(restored.page_count(), 2);
    for index in 0..2 {
        assert_eq!(
            count(&restored, index),
            ReferenceState::Referenced { references: 1 }
        );
    }
    drop(nested);
    assert_eq!(
        count(&nested_last, 0),
        ReferenceState::Referenced { references: 1 }
    );
}

#[def_test]
fn compound_tail_views_retain_the_complete_allocation_and_original_view_bounds() {
    let _guard = NoPreemptIrqSave::new();
    let kind = UsageKind::VirtMem;
    let before = counters(kind);
    let owner = alloc_compound_pages_ref(4, PAGE_SIZE_4K, kind).unwrap();
    let base = owner.phys_addr();
    let head = global_meta(owner.pfn());
    let whole = owner.try_clone_ref().unwrap();
    let tail = owner.try_clone_range(2, 2).unwrap();
    let last = tail.try_clone_page(1).unwrap();
    assert_eq!(tail.page_count(), 2);
    assert_eq!(last.page_count(), 1);
    assert_eq!(
        last.phys_addr().as_usize(),
        base.as_usize() + 3 * PAGE_SIZE_4K
    );
    assert_eq!(
        tail.retained_range(),
        PhysAddrRange::from_start_size(base, 4 * PAGE_SIZE_4K)
    );
    assert_eq!(
        head.reference_state(),
        ReferenceState::Referenced { references: 4 }
    );
    assert_eq!(count(&tail, 0), ReferenceState::Tail);
    let raw = tail.into_raw();
    assert_eq!(raw.1, 2);
    drop(owner);
    drop(whole);
    drop(last);
    assert_eq!(counters(kind), (before.0 + 4 * PAGE_SIZE_4K, before.1));
    // SAFETY: restores this same two-page compound view transfer. Its one head
    // hold retains the full block; no software/hardware content use exists.
    let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
    assert_eq!(restored.page_count(), 2);
    assert_eq!(
        restored.retained_range(),
        PhysAddrRange::from_start_size(base, 4 * PAGE_SIZE_4K)
    );
    drop(restored);
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + 4 * PAGE_SIZE_4K)
    );
    let reused = alloc_compound_pages_ref(4, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    assert_eq!(reused.phys_addr(), base);
}

#[def_test]
fn compound_split_preserves_failed_owner_and_cleans_unconsumed_children() {
    let _guard = NoPreemptIrqSave::new();
    let kind = UsageKind::PageTable;
    let before = counters(kind);
    let owner = alloc_compound_pages_ref(4, PAGE_SIZE_4K, kind).unwrap();
    let base = owner.phys_addr();
    let tail = owner.try_clone_page(2).unwrap();
    // SAFETY: no external use exists. The second typed hold must reject split
    // without changing the original selected range or any reference count.
    let (owner, error) = unsafe { owner.try_split() }.unwrap_err();
    assert_eq!(error, PageMetaError::NotExclusive);
    assert_eq!(owner.page_count(), 4);
    drop(owner);
    // SAFETY: tail now holds the sole compound reference and no external content
    // or hardware use exists. The output guard owns all children, not just tail.
    let mut pages = unsafe { tail.try_split() }.unwrap();
    assert_eq!(pages.len(), 4);
    let retained = pages.nth(2).unwrap();
    assert_eq!(
        retained.phys_addr().as_usize(),
        base.as_usize() + 2 * PAGE_SIZE_4K
    );
    assert_eq!(
        retained.retained_range(),
        PhysAddrRange::from_start_size(retained.phys_addr(), PAGE_SIZE_4K)
    );
    drop(pages);
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + 3 * PAGE_SIZE_4K)
    );
    drop(retained);
    assert_eq!(
        counters(kind),
        (before.0 + 4 * PAGE_SIZE_4K, before.1 + 4 * PAGE_SIZE_4K)
    );
}

#[def_test]
fn exact_page_transfer_needs_no_exclusivity_and_preserves_other_range_holds() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::RustHeap);
    let owner = alloc_pages_exact_ref(3, PAGE_SIZE_4K, UsageKind::RustHeap).unwrap();
    let whole = owner.try_clone_ref().unwrap();
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 3 * PAGE_SIZE_4K, before.1)
    );
    // SAFETY: exact transfer changes no geometry; both typed holders retain
    // their own per-page counts, and this fixture has no external content use.
    let mut pages = unsafe { owner.try_split() }.unwrap();
    assert_eq!(pages.len(), 3);
    let first = pages.next().unwrap();
    drop(pages);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 3 * PAGE_SIZE_4K, before.1)
    );
    drop(whole);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 3 * PAGE_SIZE_4K, before.1 + 2 * PAGE_SIZE_4K)
    );
    drop(first);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 3 * PAGE_SIZE_4K, before.1 + 3 * PAGE_SIZE_4K)
    );
}

#[def_test]
fn raw_compound_hold_prevents_split_until_exact_restoration() {
    let owner = alloc_compound_pages_ref(2, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    let raw = owner.try_clone_page(1).unwrap().into_raw();
    // SAFETY: no content/hardware use exists; the outstanding raw view must
    // count as another compound holder and prevent exclusive splitting.
    let (owner, error) = unsafe { owner.try_split() }.unwrap_err();
    assert_eq!(error, PageMetaError::NotExclusive);
    // SAFETY: consumes the exact outstanding one-page view transfer once.
    drop(unsafe { PageRef::from_raw(raw.0, raw.1) });
    // SAFETY: owner is now exclusive and no external content use was published.
    let pages = unsafe { owner.try_split() }.unwrap();
    assert_eq!(pages.len(), 2);
    drop(pages);
}

#[def_test]
fn invalid_ranges_and_compound_sizes_preserve_existing_holds() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::Global);
    for (count, align) in [(0, PAGE_SIZE_4K), (usize::MAX, PAGE_SIZE_4K), (1, 3)] {
        assert_eq!(
            alloc_pages_exact_ref(count, align, UsageKind::Global).unwrap_err(),
            AllocError::InvalidInput
        );
    }
    assert_eq!(
        alloc_compound_pages_ref(3, PAGE_SIZE_4K, UsageKind::Global).unwrap_err(),
        AllocError::InvalidInput
    );
    assert_eq!(counters(UsageKind::Global), before);
    let owner = alloc_pages_exact_ref(2, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    for (offset, count) in [(0, 0), (2, 1), (1, 2), (usize::MAX, 2)] {
        assert_eq!(
            owner.try_clone_range(offset, count).unwrap_err(),
            PageMetaError::InvalidRange
        );
    }
    for index in 0..2 {
        assert_eq!(
            count(&owner, index),
            ReferenceState::Referenced { references: 1 }
        );
    }
}

#[def_test]
fn zeroed_exact_range_and_raw_round_trip_preserve_selected_length_and_contents() {
    let owner = alloc_zeroed_pages_exact_ref(3, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    let base = owner.phys_addr();
    assert_eq!(owner.page_count(), 3);
    let address = kaddr_layout::p2v(base.as_usize()) as *mut u8;
    // SAFETY: the new exclusive owner retains three fully mapped initialized
    // pages; zeroing completed before the range escaped and no aliases exist.
    let bytes = unsafe { core::slice::from_raw_parts(address, 3 * PAGE_SIZE_4K) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    // SAFETY: the shared borrow above ended and the unpublished range remains
    // exclusive. The last selected byte lies within its writable direct map.
    unsafe { address.wrapping_add(3 * PAGE_SIZE_4K - 1).write(0xa5) };
    let raw = owner.into_raw();
    assert_eq!(raw, (base, 3));
    // SAFETY: restores this exact three-page range transfer with no external use.
    let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
    assert_eq!(restored.page_count(), 3);
    // SAFETY: restoration retains the same initialized allocation with no
    // conflicting writes; the selected last-byte offset remains valid.
    let last = unsafe { address.wrapping_add(3 * PAGE_SIZE_4K - 1).read() };
    assert_eq!(last, 0xa5);
}

#[def_test]
fn power_of_two_exact_range_releases_pages_outside_a_retained_subrange() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::Global);
    let owner = alloc_pages_exact_ref(8, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    let middle = owner.try_clone_range(2, 3).unwrap();
    drop(owner);
    assert_eq!(
        counters(UsageKind::Global),
        (before.0 + 8 * PAGE_SIZE_4K, before.1 + 5 * PAGE_SIZE_4K)
    );
    assert_eq!(middle.retained_range().size(), 3 * PAGE_SIZE_4K);
    for index in 0..3 {
        assert_eq!(
            count(&middle, index),
            ReferenceState::Referenced { references: 1 }
        );
    }
    drop(middle);
    assert_eq!(
        counters(UsageKind::Global),
        (before.0 + 8 * PAGE_SIZE_4K, before.1 + 8 * PAGE_SIZE_4K)
    );
}

#[def_test]
fn aligned_exact_and_compound_allocations_preserve_geometry_and_accounting() {
    let _guard = NoPreemptIrqSave::new();
    for kind in [
        UsageKind::RustHeap,
        UsageKind::VirtMem,
        UsageKind::PageCache,
        UsageKind::PageTable,
        UsageKind::Dma,
        UsageKind::Global,
    ] {
        for alignment in [4 * PAGE_SIZE_4K, 16 * PAGE_SIZE_4K] {
            let before = counters(kind);
            let exact = alloc_pages_exact_ref(3, alignment, kind).unwrap();
            let compound = alloc_compound_pages_ref(4, alignment, kind).unwrap();
            assert_eq!(exact.phys_addr().as_usize() % alignment, 0);
            assert_eq!(compound.phys_addr().as_usize() % alignment, 0);
            assert_eq!(exact.page_count(), 3);
            assert_eq!(compound.page_count(), 4);
            drop(exact);
            drop(compound);
            let after = counters(kind);
            assert_eq!(after.0 - before.0, after.1 - before.1);
        }
    }
}

#[def_test]
fn order_zero_compound_and_exact_support_the_same_raw_transfer_and_split() {
    for compound in [false, true] {
        let owner = if compound {
            alloc_compound_pages_ref(1, PAGE_SIZE_4K, UsageKind::Global).unwrap()
        } else {
            alloc_page_ref(UsageKind::Global).unwrap()
        };
        let copy = owner.try_clone_ref().unwrap();
        let raw = owner.into_raw();
        // SAFETY: consumes this exact one-page transfer once; copy keeps another
        // hold alive, and this allocation has no content or hardware users.
        let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
        // SAFETY: an order-zero transfer changes no geometry or content access,
        // so the additional independent hold does not prevent splitting.
        let mut pages = unsafe { restored.try_split() }.unwrap();
        let child = pages.next().unwrap();
        assert_eq!(child.phys_addr(), copy.phys_addr());
        assert_eq!(pages.len(), 0);
        drop(copy);
        assert_eq!(
            count(&child, 0),
            ReferenceState::Referenced { references: 1 }
        );
    }
}

#[def_test]
fn large_zeroed_exact_backing_releases_rounding_and_unselected_pages() {
    let _guard = NoPreemptIrqSave::new();
    const SELECTED: usize = 513;
    let before = counters(UsageKind::Global);
    let owner = alloc_zeroed_pages_exact_ref(SELECTED, PAGE_SIZE_4K, UsageKind::Global).unwrap();
    assert_eq!(owner.page_count(), SELECTED);
    let address = kaddr_layout::p2v(owner.phys_addr().as_usize()) as *const u8;
    // SAFETY: the new exclusive owner retains SELECTED initialized, contiguous
    // direct-mapped pages. No writable alias or hardware user was published.
    let bytes = unsafe { core::slice::from_raw_parts(address, SELECTED * PAGE_SIZE_4K) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    let last = owner.try_clone_page(SELECTED - 1).unwrap();
    drop(owner);
    let held = counters(UsageKind::Global);
    assert_eq!((held.0 - before.0) - (held.1 - before.1), PAGE_SIZE_4K);
    assert_eq!(last.retained_range().size(), PAGE_SIZE_4K);
    drop(last);
    let after = counters(UsageKind::Global);
    assert_eq!(after.0 - before.0, after.1 - before.1);
}
