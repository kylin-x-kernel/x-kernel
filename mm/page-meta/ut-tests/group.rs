// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddrRange};
use unittest::def_test;

use super::*;
use crate::ReferenceState;

#[repr(align(4096))]
struct Storage([u8; PAGE]);

/// Builds a local directory without publishing or touching global inventory.
///
/// # Safety
/// Retain exclusive storage until the table and every descriptor borrow end.
unsafe fn table(storage: &mut Storage) -> PageMetaTable {
    let pointer = NonNull::from(&mut storage.0).cast();
    let mut free = [
        PhysAddrRange::from_start_size((256 * PAGE).into(), 16 * PAGE),
        PhysAddrRange::from_start_size((512 * PAGE).into(), 32 * PAGE),
    ];
    // SAFETY: the fixture exclusively owns this aligned writable buffer, which
    // covers all 48 descriptors and outlives the returned table and its borrows.
    unsafe { crate::init(&mut free, |_| Ok(pointer)) }.unwrap()
}

#[def_test]
fn tails_resolve_the_head_and_retirement_clears_membership_before_reuse() {
    let mut storage = Storage([0xa5; PAGE]);
    // SAFETY: storage is exclusively retained through the table and all holds.
    let table = unsafe { table(&mut storage) };
    // SAFETY: these four modeled pages are exclusive, unpublished and nonreserved.
    let head = unsafe { table.claim_reference_group(260, 2, UsageKind::VirtMem) }.unwrap();
    assert_eq!(
        head.reference_state(),
        ReferenceState::Referenced { references: 1 }
    );
    for pfn in 260..264 {
        let member = table.lookup(pfn).unwrap();
        // SAFETY: the fixture's initial whole-allocation hold remains live.
        let allocation = unsafe { table.reference_allocation(member) }.unwrap();
        assert_eq!(allocation.head_pfn(), 260);
        assert_eq!(allocation.order(), 2);
        assert_eq!(allocation.page_count(), 4);
        if pfn != 260 {
            assert_eq!(member.reference_state(), ReferenceState::Tail);
        }
    }
    // SAFETY: retains the initial hold while adding another whole hold.
    unsafe { head.clone_reference() }.unwrap();
    // SAFETY: consumes the added whole hold with no modeled hardware use.
    assert!(!unsafe { head.release_reference() });
    // SAFETY: consumes the last whole hold.
    assert!(unsafe { head.release_reference() });
    // SAFETY: owns the unique retirement before modeled inventory reuse.
    let kind = unsafe { table.finish_reference_retirement(260) };
    assert_eq!(kind, UsageKind::VirtMem);
    for pfn in 260..264 {
        assert_eq!(
            table.lookup(pfn).unwrap().reference_state(),
            ReferenceState::Unreferenced
        );
    }
    let former_tail = table.lookup(262).unwrap();
    // SAFETY: models exclusive reallocation of a former tail after retirement.
    unsafe { former_tail.claim_reference(UsageKind::PageTable) }.unwrap();
    // SAFETY: consumes that independent page's sole modeled hold.
    assert!(unsafe { former_tail.release_reference() });
    // SAFETY: independent order-zero retirement was selected above.
    let kind = unsafe { former_tail.finish_retirement() };
    assert_eq!(kind, UsageKind::PageTable);
}

#[def_test]
fn split_requires_exclusivity_and_turns_every_child_into_its_own_head() {
    let mut storage = Storage([0xa5; PAGE]);
    // SAFETY: retains exclusive aligned backing throughout this fixture.
    let table = unsafe { table(&mut storage) };
    // SAFETY: this is an exclusive new modeled four-page allocation.
    let head = unsafe { table.claim_reference_group(260, 2, UsageKind::PageCache) }.unwrap();
    // SAFETY: the initial hold remains live while acquiring the second.
    unsafe { head.clone_reference() }.unwrap();
    // SAFETY: the caller retains a hold and has no hardware use; the count check
    // must reject the modeled second holder without modifying either hold.
    let result = unsafe { table.split_reference(head) };
    assert_eq!(result, Err(PageMetaError::NotExclusive));
    assert_eq!(
        head.reference_state(),
        ReferenceState::Referenced { references: 2 }
    );
    // SAFETY: consumes the second hold before retrying exclusive split.
    assert!(!unsafe { head.release_reference() });
    let tail = table.lookup(262).unwrap();
    // SAFETY: the remaining whole hold selects a tail, is exclusive, and has no
    // content/hardware use. The fixture consumes every child below on success.
    let old = unsafe { table.split_reference(tail) }.unwrap();
    assert_eq!(old.head_pfn(), 260);
    assert_eq!(old.page_count(), 4);
    for pfn in 260..264 {
        let child = table.lookup(pfn).unwrap();
        // SAFETY: the successful split supplied one independent child hold.
        let allocation = unsafe { table.reference_allocation(child) }.unwrap();
        assert_eq!(allocation.head_pfn(), pfn);
        assert_eq!(allocation.order(), 0);
        assert_eq!(
            child.reference_state(),
            ReferenceState::Referenced { references: 1 }
        );
        // SAFETY: consumes exactly this child's hold, with no external use.
        assert!(unsafe { child.release_reference() });
        // SAFETY: this child is an independently retired order-zero allocation.
        let kind = unsafe { table.finish_reference_retirement(pfn) };
        assert_eq!(kind, UsageKind::PageCache);
    }
}

#[def_test]
fn admission_errors_do_not_leave_a_claimed_prefix_or_change_reserved_pages() {
    let mut storage = Storage([0xa5; PAGE]);
    // SAFETY: storage remains exclusively live through all table accesses.
    let table = unsafe { table(&mut storage) };
    // SAFETY: models exclusive input; admission must reject the physical hole
    // before changing any covered descriptor.
    let result = unsafe { table.claim_reference_group(270, 2, UsageKind::Global) };
    assert!(matches!(result, Err(PageMetaError::NotCovered)));
    for pfn in 270..272 {
        assert_eq!(
            table.lookup(pfn).unwrap().reference_state(),
            ReferenceState::Unreferenced
        );
    }
    // SAFETY: this negative fixture exclusively models the metadata reservation;
    // admission must reject it without acquiring any hold.
    let result = unsafe { table.claim_reference_group(543, 0, UsageKind::Global) };
    assert!(matches!(result, Err(PageMetaError::InvalidState)));
    assert_eq!(
        table.lookup(543).unwrap().reference_state(),
        ReferenceState::Permanent
    );
    // SAFETY: invalid geometry is rejected before touching modeled descriptors.
    let result = unsafe { table.claim_reference_group(260, u8::MAX, UsageKind::Global) };
    assert!(matches!(result, Err(PageMetaError::InvalidRange)));
}

#[def_test]
fn independent_range_clone_rolls_back_acquired_prefix_on_counter_overflow() {
    let mut storage = Storage([0xa5; PAGE]);
    // SAFETY: fixture storage stays exclusive and outlives the local directory.
    let table = unsafe { table(&mut storage) };
    // SAFETY: all six modeled pages are new, exclusive, unpublished and covered.
    let first = unsafe { table.claim_reference_range(260, 6, UsageKind::PageCache) }.unwrap();
    let full = table.lookup(263).unwrap();
    // Model overflow only in this isolated fixture, then restore the original
    // single real hold before retiring its modeled storage.
    full.references.store(MAX_REFERENCES, Ordering::Relaxed);
    // SAFETY: the fixture retains its original hold on every independent page;
    // the failed clone must roll back its first three successful increments.
    let result = unsafe { table.clone_reference_range(first, 6) };
    assert_eq!(result, Err(PageMetaError::ReferenceOverflow));
    for pfn in 260..266 {
        let expected = if pfn == 263 { MAX_REFERENCES } else { 1 };
        assert_eq!(
            table
                .lookup(pfn)
                .unwrap()
                .references
                .load(Ordering::Relaxed),
            expected
        );
    }
    full.references.store(1, Ordering::Relaxed);
    // SAFETY: the fixture still retains the complete original range after the
    // rollback, with no mapping or hardware use and stable independent geometry.
    let unit = unsafe { table.reference_range(first, 6) }.unwrap();
    assert_eq!(unit.order(), 0);
    for pfn in 260..266 {
        let page = table.lookup(pfn).unwrap();
        // SAFETY: consumes exactly the original independent page hold.
        assert!(unsafe { page.release_reference() });
        // SAFETY: this fixture owns that child's unique retirement obligation.
        let kind = unsafe { table.finish_reference_retirement(pfn) };
        assert_eq!(kind, UsageKind::PageCache);
    }
}

#[def_test]
fn independent_admission_rejects_a_hole_without_claiming_covered_prefix() {
    let mut storage = Storage([0xa5; PAGE]);
    // SAFETY: fixture storage remains exclusively live through all operations.
    let table = unsafe { table(&mut storage) };
    // SAFETY: exclusive modeled input intentionally includes an uncovered suffix;
    // range admission must reject the geometry before changing any descriptor.
    let result = unsafe { table.claim_reference_range(270, 4, UsageKind::Global) };
    assert!(matches!(result, Err(PageMetaError::NotCovered)));
    for pfn in 270..272 {
        assert_eq!(
            table.lookup(pfn).unwrap().reference_state(),
            ReferenceState::Unreferenced
        );
    }
}
