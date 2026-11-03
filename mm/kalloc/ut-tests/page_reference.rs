// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::Ordering;

use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K as PAGE;
use page_meta::ReferenceState;
use unittest::def_test;

use crate::{PageRef, UsageKind, alloc_page_ref, alloc_zeroed_page_ref};

fn kernel_addr(frame: &PageRef) -> memaddr::VirtAddr {
    kaddr_layout::p2v(frame.phys_addr().as_usize()).into()
}

fn counters(kind: UsageKind) -> (usize, usize) {
    crate::with_current_usage_counters(|counters| {
        (
            counters.allocated_bytes[kind as usize].load(Ordering::Relaxed),
            counters.freed_bytes[kind as usize].load(Ordering::Relaxed),
        )
    })
}

#[def_test]
fn shared_page_returns_to_global_inventory_only_after_the_last_hold() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::Global);
    let owner = alloc_page_ref(UsageKind::Global).unwrap();
    let va = kernel_addr(&owner).as_usize();
    let meta = page_meta::lookup(owner.pfn()).unwrap();
    let second = owner.try_clone_ref().unwrap();
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 2 }
    );
    drop(owner);
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 1 }
    );
    assert_eq!(counters(UsageKind::Global), (before.0 + PAGE, before.1));
    drop(second);
    // Allocation removed one PCP slot. No allocation, migration or local IRQ
    // intervened, so final release fits that slot and remote CPUs cannot reuse
    // this page before the state/accounting snapshot and next allocation.
    assert_eq!(meta.reference_state(), ReferenceState::Unreferenced);
    assert_eq!(
        counters(UsageKind::Global),
        (before.0 + PAGE, before.1 + PAGE)
    );
    let reused = alloc_page_ref(UsageKind::Global).unwrap();
    assert_eq!(kernel_addr(&reused).as_usize(), va);
}

#[def_test]
fn zeroed_pages_and_heap_accounting_use_the_shared_lifetime_protocol() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::RustHeap);
    let original = alloc_page_ref(UsageKind::RustHeap).unwrap();
    // SAFETY: original is the only unpublished hold of this writable page;
    // its full direct-map range is valid and no content alias has escaped.
    unsafe { core::ptr::write_bytes(kernel_addr(&original).as_mut_ptr(), 0xa5, PAGE) };
    let pa = original.phys_addr();
    drop(original);
    let zeroed = alloc_zeroed_page_ref(UsageKind::RustHeap).unwrap();
    // The guard retains the just-freed page in the local PCP slot until reuse.
    assert_eq!(zeroed.phys_addr(), pa);
    // SAFETY: the unique unpublished owner retains initialized bytes in this
    // one-page range. This borrow ends before cloning or dropping the owner.
    let bytes = unsafe { core::slice::from_raw_parts(kernel_addr(&zeroed).as_ptr(), PAGE) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    let second = zeroed.try_clone_ref().unwrap();
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 2 * PAGE, before.1 + PAGE)
    );
    drop(zeroed);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 2 * PAGE, before.1 + PAGE)
    );
    drop(second);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 2 * PAGE, before.1 + 2 * PAGE)
    );
}

#[def_test]
fn raw_transfer_preserves_one_hold_and_its_accounting_category() {
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::PageTable);
    let owner = alloc_page_ref(UsageKind::PageTable).unwrap();
    let meta = page_meta::lookup(owner.pfn()).unwrap();
    let observer = owner.try_clone_ref().unwrap();
    let raw = owner.into_raw();
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 2 }
    );
    // SAFETY: consumes this exact outstanding transfer once; the original
    // accounting class stays in metadata. The fixture never uses hardware.
    let restored = unsafe { PageRef::from_raw(raw.0, raw.1) };
    assert_eq!(restored.phys_addr(), raw.0);
    assert_eq!(raw.1, 1);
    drop(restored);
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 1 }
    );
    assert_eq!(counters(UsageKind::PageTable), (before.0 + PAGE, before.1));
    drop(observer);
    assert_eq!(
        counters(UsageKind::PageTable),
        (before.0 + PAGE, before.1 + PAGE)
    );
}

#[def_test]
fn reused_page_reclaims_each_allocation_with_its_original_category() {
    let _guard = NoPreemptIrqSave::new();
    let original_before = counters(UsageKind::VirtMem);
    let reused_before = counters(UsageKind::PageTable);
    let original = alloc_page_ref(UsageKind::VirtMem).unwrap();
    let pa = original.phys_addr();
    drop(original);
    assert_eq!(
        counters(UsageKind::VirtMem),
        (original_before.0 + PAGE, original_before.1 + PAGE)
    );
    // The same CPU's just-freed slot remains available until the next request.
    let reused = alloc_page_ref(UsageKind::PageTable).unwrap();
    assert_eq!(reused.phys_addr(), pa);
    drop(reused);
    assert_eq!(
        counters(UsageKind::PageTable),
        (reused_before.0 + PAGE, reused_before.1 + PAGE)
    );
    assert_eq!(
        counters(UsageKind::VirtMem),
        (original_before.0 + PAGE, original_before.1 + PAGE)
    );
}
