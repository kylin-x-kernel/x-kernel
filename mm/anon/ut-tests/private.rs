// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{
    PageRef, UsageKind, alloc_compound_pages_ref, alloc_page_ref, alloc_zeroed_page_ref,
    alloc_zeroed_pages_exact_ref,
};
use kerrno::KError;
use khal::paging::PageSize;
use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K;
use page_meta::ReferenceState;
use unittest::def_test;
use vmobj::{MappingViewKind, MappingViewSpec};

use super::{AnonPrivateObject, AnonPrivatePageCommitError};

fn new_page() -> PageRef {
    alloc_zeroed_page_ref(UsageKind::VirtMem).expect("allocate real private backing")
}

fn reference_state(page: &PageRef) -> ReferenceState {
    page_meta::lookup(page.pfn())
        .expect("allocated page metadata")
        .reference_state()
}

fn assert_last_hold_returns_to_inventory(page: PageRef) {
    let _guard = NoPreemptIrqSave::new();
    // Reserve a PCP slot immediately before final release. No heap allocation,
    // migration or local IRQ intervenes before the state and reuse snapshots.
    let _pcp_space = alloc_page_ref(UsageKind::VirtMem).expect("reserve PCP space");
    let pa = page.phys_addr();
    let meta = page_meta::lookup(page.pfn()).expect("last hold metadata");
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 1 }
    );
    drop(page);
    assert_eq!(meta.reference_state(), ReferenceState::Unreferenced);
    let reused = alloc_page_ref(UsageKind::VirtMem).expect("reuse final released page");
    assert_eq!(reused.phys_addr(), pa);
}

#[def_test]
fn anon_private_fork_child_keeps_lineage_and_gets_new_identity() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    assert_eq!(parent.lineage(), child.lineage());
    assert_ne!(parent.id(), child.id());
    assert_ne!(parent.anon_id(), child.anon_id());
}

#[def_test]
fn anon_private_registers_views_and_emits_object_hits() {
    let object = AnonPrivateObject::new_root();
    let _view = object.register_view(MappingViewSpec {
        mm_id: 9,
        vma_start: 0x9000,
        vma_len: 0x3000,
        object_start: 0,
        object_len: 0x3000,
        kind: MappingViewKind::Private,
        notifier: None,
    });
    let work = object
        .invalidate_range(0x1000, 0x1000)
        .expect("private anon hit");
    assert_eq!(work.hits().len(), 1);
    let hit = &work.hits()[0];
    assert_eq!(hit.view().mm_id(), 9);
    assert_eq!(hit.vma_start(), 0xa000);
    assert_eq!(hit.object_start(), 0x1000);
    assert_eq!(hit.object_len(), 0x1000);
}

#[def_test]
fn anon_private_observers_and_pte_holds_do_not_create_logical_shares() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0x1000, new_page(), PageSize::Size4K)
        .unwrap();
    let observer = handle.clone();
    let pte = handle.try_clone_mapping_ref().unwrap();
    assert!(handle.is_exclusive());
    assert!(observer.is_exclusive());
    assert_eq!(
        reference_state(&pte),
        ReferenceState::Referenced { references: 2 }
    );

    let prepared = parent
        .prepare_fork_child_pages(0x1000, PAGE_SIZE_4K, &child)
        .unwrap();
    assert_eq!(prepared.pages().len(), 1);
    assert_eq!(prepared.pages()[0].object_start(), 0x1000);
    assert!(!observer.is_exclusive());
    // Child logical ownership shares the immutable identity's one PageRef.
    assert_eq!(
        reference_state(&pte),
        ReferenceState::Referenced { references: 2 }
    );
    prepared.commit().unwrap();
    assert_eq!(
        child.page_at(0x1000).unwrap().phys_addr(),
        handle.phys_addr()
    );

    parent.remove_range(0x1000, PAGE_SIZE_4K);
    assert!(observer.is_exclusive());
    child.remove_range(0x1000, PAGE_SIZE_4K);
    assert!(!observer.is_exclusive());
    drop(observer);
    drop(handle);
    assert_last_hold_returns_to_inventory(pte);
}

#[def_test]
fn anon_private_first_touch_commits_typed_backing() {
    let object = AnonPrivateObject::new_root();
    let prepared = object.prepare_first_touch_page(0x1000).unwrap();
    assert!(object.page_at(0x1000).is_none());
    let page = new_page();
    let pa = page.phys_addr();
    let handle = prepared.commit(page, PageSize::Size4K).unwrap();
    assert_eq!(handle.phys_addr(), pa);
    assert_eq!(handle.page_size(), PageSize::Size4K);
    assert_eq!(object.page_at(0x1000).unwrap().phys_addr(), pa);
}

#[def_test]
fn anon_private_first_touch_callback_gets_candidate_before_publication() {
    let object = AnonPrivateObject::new_root();
    let page = new_page();
    let pa = page.phys_addr();
    let mut mapped = None;
    let handle = object
        .prepare_first_touch_page(0)
        .unwrap()
        .commit_after(page, PageSize::Size4K, |candidate| {
            assert_eq!(candidate.phys_addr(), pa);
            assert!(candidate.is_exclusive());
            mapped = Some(candidate.try_clone_mapping_ref()?);
            Ok::<_, KError>(())
        })
        .unwrap();
    assert_eq!(object.page_at(0).unwrap().phys_addr(), pa);
    drop(handle);
    drop(object);
    assert_last_hold_returns_to_inventory(mapped.unwrap());
}

#[def_test]
fn anon_private_first_touch_commit_revalidates_empty_slot() {
    let object = AnonPrivateObject::new_root();
    let prepared = object.prepare_first_touch_page(0x1000).unwrap();
    let original = object
        .install_page(0x1000, new_page(), PageSize::Size4K)
        .unwrap();
    let candidate = new_page();
    let witness = candidate.try_clone_ref().unwrap();
    let result = prepared.commit(candidate, PageSize::Size4K);
    assert!(matches!(result, Err(KError::AlreadyExists)));
    assert_eq!(
        object.page_at(0x1000).unwrap().phys_addr(),
        original.phys_addr()
    );
    assert_last_hold_returns_to_inventory(witness);
}

#[def_test]
fn anon_private_page_size_uses_the_selected_extent_for_exact_and_compound_pages() {
    let size = PageSize::Size2M;
    let count = size as usize / PAGE_SIZE_4K;
    let object = AnonPrivateObject::new_root();
    let exact = alloc_zeroed_pages_exact_ref(count, size as usize, UsageKind::VirtMem).unwrap();
    let exact = object.prepare_first_touch_page(0).unwrap().commit(exact, size).unwrap();
    assert_eq!(exact.page_size(), size);
    assert_eq!(exact.try_clone_mapping_ref().unwrap().page_count(), count);

    let compound = alloc_compound_pages_ref(count, size as usize, UsageKind::VirtMem).unwrap();
    let selected = compound.try_clone_page(1).unwrap();
    assert_eq!(selected.page_count(), 1);
    assert_eq!(selected.retained_range().size(), size as usize);
    let small = object.prepare_first_touch_page(size as u64).unwrap()
        .commit(selected, PageSize::Size4K).unwrap();
    assert_eq!(small.page_size(), PageSize::Size4K);
    assert_eq!(small.try_clone_mapping_ref().unwrap().page_count(), 1);

    let whole = object.prepare_first_touch_page(2 * size as u64).unwrap()
        .commit(compound, size).unwrap();
    assert_eq!(whole.page_size(), size);
    object.remove_range(0, 3 * size as usize);
    assert_eq!(small.page_size(), PageSize::Size4K);
    assert_eq!(whole.page_size(), size);
}

#[def_test]
fn anon_private_invalid_geometry_is_rejected_before_callback() {
    let object = AnonPrivateObject::new_root();
    let page = new_page();
    let witness = page.try_clone_ref().unwrap();
    let result = object
        .prepare_first_touch_page(0)
        .unwrap()
        .commit(page, PageSize::Size2M);
    assert!(matches!(result, Err(KError::InvalidInput)));
    assert_eq!(
        reference_state(&witness),
        ReferenceState::Referenced { references: 1 }
    );
    let mut did_run = false;
    let page = alloc_zeroed_pages_exact_ref(2, PAGE_SIZE_4K, UsageKind::VirtMem).unwrap();
    let result =
        object
            .prepare_first_touch_page(0)
            .unwrap()
            .commit_after(page, PageSize::Size4K, |_| {
                did_run = true;
                Ok::<_, ()>(())
            });
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::InvalidPage)
    ));
    assert!(!did_run);
    assert!(object.page_at(0).is_none());
    assert_last_hold_returns_to_inventory(witness);
}

#[def_test]
fn anon_private_invalid_alignment_is_rejected_for_a_matching_extent() {
    let object = AnonPrivateObject::new_root();
    let size = PageSize::Size2M;
    let count = size as usize / PAGE_SIZE_4K;
    let backing = alloc_zeroed_pages_exact_ref(count + 1, size as usize, UsageKind::VirtMem)
        .expect("allocate aligned extent with an independent trailing page");
    let candidate = backing.try_clone_range(1, count).unwrap();
    assert_eq!(candidate.page_count() * PAGE_SIZE_4K, size as usize);
    assert!(
        !candidate
            .phys_addr()
            .as_usize()
            .is_multiple_of(size as usize)
    );
    let mut did_run = false;
    let result = object
        .prepare_first_touch_page(0)
        .unwrap()
        .commit_after(candidate, size, |_| {
            did_run = true;
            Ok::<_, ()>(())
        });
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::InvalidPage)
    ));
    assert!(!did_run);
    assert!(object.page_at(0).is_none());
    let first_rejected_page = backing.try_clone_range(1, 1).unwrap();
    assert_eq!(
        reference_state(&first_rejected_page),
        ReferenceState::Referenced { references: 2 }
    );
}

#[def_test]
fn anon_private_first_cow_callback_failure_and_changed_slot_release_candidate() {
    let object = AnonPrivateObject::new_root();
    let page = new_page();
    let witness = page.try_clone_ref().unwrap();
    let result = object.prepare_first_touch_page(0).unwrap().commit_after(
        page,
        PageSize::Size4K,
        |candidate| {
            let _mapping = candidate.try_clone_mapping_ref().unwrap();
            Err::<(), _>(17)
        },
    );
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::Commit(17))
    ));
    assert!(object.page_at(0).is_none());
    assert_last_hold_returns_to_inventory(witness);

    let prepared = object.prepare_first_touch_page(0).unwrap();
    let original = object
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let page = new_page();
    let witness = page.try_clone_ref().unwrap();
    let mut did_run = false;
    let result = prepared.commit_after(page, PageSize::Size4K, |_| {
        did_run = true;
        Ok::<_, ()>(())
    });
    assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
    assert!(!did_run);
    assert_eq!(object.page_at(0).unwrap().phys_addr(), original.phys_addr());
    assert_last_hold_returns_to_inventory(witness);
}

#[def_test]
fn anon_private_retired_logical_slot_blocks_sibling_exclusive_reuse() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let old = parent
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    parent
        .prepare_fork_child_pages(0, PAGE_SIZE_4K, &child)
        .unwrap()
        .commit()
        .unwrap();
    let sibling = child.page_at(0).unwrap();
    let old_pte = old.try_clone_mapping_ref().unwrap();
    let mut new_pte = None;
    let retired = parent
        .replace_page_if_same_after(0, &old, new_page(), PageSize::Size4K, |candidate| {
            new_pte = Some(candidate.try_clone_mapping_ref()?);
            Ok::<_, KError>(())
        })
        .unwrap();
    assert_eq!(retired.handle().phys_addr(), sibling.phys_addr());
    assert!(!sibling.is_exclusive());
    drop(old);
    assert!(
        !sibling.is_exclusive(),
        "observers cannot end retired logical shares"
    );
    drop(old_pte); // Model confirmed old-owner PTE retirement before logical release.
    assert!(!sibling.is_exclusive());
    drop(retired);
    assert!(sibling.is_exclusive());
    drop(sibling);
    drop(child);
    drop(parent);
    assert_last_hold_returns_to_inventory(new_pte.unwrap());
}

#[def_test]
fn anon_private_replacement_errors_preserve_expected_slot() {
    let object = AnonPrivateObject::new_root();
    let old = object
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let page = new_page();
    let witness = page.try_clone_ref().unwrap();
    let result = object.replace_page_if_same_after(0, &old, page, PageSize::Size4K, |candidate| {
        let _mapping = candidate.try_clone_mapping_ref().unwrap();
        Err::<(), _>(23)
    });
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::Commit(23))
    ));
    assert_eq!(object.page_at(0).unwrap().phys_addr(), old.phys_addr());
    assert!(old.is_exclusive());
    assert_last_hold_returns_to_inventory(witness);

    let mut did_run = false;
    let result = object.replace_page_if_same_after(0, &old, new_page(), PageSize::Size2M, |_| {
        did_run = true;
        Ok::<_, ()>(())
    });
    assert!(matches!(
        result,
        Err(AnonPrivatePageCommitError::InvalidPage)
    ));
    assert!(!did_run);
    assert_eq!(object.page_at(0).unwrap().phys_addr(), old.phys_addr());
}

#[def_test]
fn anon_private_replace_if_same_rejects_changed_slot() {
    let object = AnonPrivateObject::new_root();
    let expected = object
        .install_page(0x1000, new_page(), PageSize::Size4K)
        .unwrap();
    let retired = object
        .replace_page_if_same_after(0x1000, &expected, new_page(), PageSize::Size4K, |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    drop(retired); // No fixture PTEs were published for this old slot.
    let competing_pa = object.page_at(0x1000).unwrap().phys_addr();
    let page = new_page();
    let witness = page.try_clone_ref().unwrap();
    let mut did_run = false;
    let result =
        object.replace_page_if_same_after(0x1000, &expected, page, PageSize::Size4K, |_| {
            did_run = true;
            Ok::<_, ()>(())
        });
    assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
    assert!(!did_run);
    assert_eq!(object.page_at(0x1000).unwrap().phys_addr(), competing_pa);
    assert_last_hold_returns_to_inventory(witness);
}

#[def_test]
fn anon_private_remove_range_preserves_slots_outside_sparse_range() {
    let object = AnonPrivateObject::new_root();
    let first = object
        .install_page(0x1000, new_page(), PageSize::Size4K)
        .unwrap();
    let middle = object
        .install_page(0x3000, new_page(), PageSize::Size4K)
        .unwrap();
    let last = object
        .install_page(0x7000, new_page(), PageSize::Size4K)
        .unwrap();
    // This object has no mappings or other content accesses to retire.
    object.remove_range(0x2000, 0x3000);
    assert!(object.page_at(0x3000).is_none());
    assert!(!middle.is_exclusive(), "the removed slot has no logical owner");
    assert_eq!(object.page_at(0x1000).unwrap().phys_addr(), first.phys_addr());
    assert_eq!(object.page_at(0x7000).unwrap().phys_addr(), last.phys_addr());
    object.remove_range(0x1000, 0);
    assert!(first.is_exclusive());
    assert!(last.is_exclusive());
}

#[def_test]
fn anon_private_slots_stay_shared_until_post_retirement_removal() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let old = parent
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    parent
        .prepare_fork_child_pages(0, PAGE_SIZE_4K, &child)
        .unwrap()
        .commit()
        .unwrap();
    assert!(!old.is_exclusive());
    assert!(parent.page_at(0).is_some());
    // Model the MM's access-retirement boundary before logical removal.
    parent.remove_range(0, PAGE_SIZE_4K);
    assert!(parent.page_at(0).is_none());
    assert!(old.is_exclusive());
    drop(child);
    assert!(!old.is_exclusive());
}

#[def_test]
fn anon_private_fork_prepare_rolls_back_without_commit() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let prepared = parent
        .prepare_fork_child_pages(0, PAGE_SIZE_4K, &child)
        .unwrap();
    assert!(!handle.is_exclusive());
    drop(prepared); // Unpublished child has no runnable mapping or content access.
    assert!(handle.is_exclusive());
    assert!(child.page_at(0).is_none());
}

#[def_test]
fn anon_private_fork_logical_overflow_unwinds_earlier_reservations() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let first = parent
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let overflow = parent
        .install_page(0x1000, new_page(), PageSize::Size4K)
        .unwrap();
    // Reach logical overflow without manufacturing physical-reference ownership.
    *overflow.inner.logical_shares.lock() = u32::MAX;
    let result = parent.prepare_fork_child_pages(0, 2 * PAGE_SIZE_4K, &child);
    let shares_after = *overflow.inner.logical_shares.lock();
    *overflow.inner.logical_shares.lock() = 1;
    assert!(matches!(result, Err(KError::NoMemory)));
    assert_eq!(shares_after, u32::MAX);
    assert!(first.is_exclusive());
    assert!(overflow.is_exclusive());
    assert!(child.page_at(0).is_none());
}

#[def_test]
fn anon_private_fork_child_collision_releases_only_reserved_slots() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let source = parent
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let child_page = child.install_page(0, new_page(), PageSize::Size4K).unwrap();
    assert!(matches!(
        parent.prepare_fork_child_pages(0, PAGE_SIZE_4K, &child),
        Err(KError::AlreadyExists)
    ));
    assert!(source.is_exclusive());
    assert_eq!(
        child.page_at(0).unwrap().phys_addr(),
        child_page.phys_addr()
    );

    let empty_child = parent.fork_child();
    let prepared = parent
        .prepare_fork_child_pages(0, PAGE_SIZE_4K, &empty_child)
        .unwrap();
    let competing = empty_child
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    assert_eq!(prepared.commit(), Err(KError::AlreadyExists));
    assert!(source.is_exclusive());
    assert_eq!(
        empty_child.page_at(0).unwrap().phys_addr(),
        competing.phys_addr()
    );
}

#[def_test]
fn anon_private_object_and_mapping_holds_release_independently() {
    let object = AnonPrivateObject::new_root();
    let handle = object
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let pte = handle.try_clone_mapping_ref().unwrap();
    drop(handle);
    drop(object); // No content access remains; only the independent lifetime hold.
    assert_last_hold_returns_to_inventory(pte);

    let object = AnonPrivateObject::new_root();
    let handle = object
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    let pte = handle.try_clone_mapping_ref().unwrap();
    drop(pte);
    let witness = handle.try_clone_mapping_ref().unwrap();
    assert_eq!(
        reference_state(&witness),
        ReferenceState::Referenced { references: 2 }
    );
    drop(handle);
    assert_eq!(
        reference_state(&witness),
        ReferenceState::Referenced { references: 2 }
    );
    drop(object);
    assert_last_hold_returns_to_inventory(witness);
}

#[def_test]
fn anon_private_observer_keeps_storage_after_last_logical_slot() {
    let object = AnonPrivateObject::new_root();
    let observer = object
        .install_page(0, new_page(), PageSize::Size4K)
        .unwrap();
    drop(object);
    assert!(!observer.is_exclusive());
    let pte = observer.try_clone_mapping_ref().unwrap();
    assert_eq!(
        reference_state(&pte),
        ReferenceState::Referenced { references: 2 }
    );
    drop(observer);
    assert_last_hold_returns_to_inventory(pte);
}

#[def_test]
fn post_unmap_removal_preserves_fork_slots_and_independent_physical_holds() {
    let parent = AnonPrivateObject::new_root();
    let shared = parent.install_page(0, new_page(), PageSize::Size4K).unwrap();
    let private = parent.install_page(0x1000, new_page(), PageSize::Size4K).unwrap();
    let shared_pte = shared.try_clone_mapping_ref().unwrap();
    let private_pte = private.try_clone_mapping_ref().unwrap();
    let child = parent.fork_child();
    parent.prepare_fork_child_pages(0, PAGE_SIZE_4K, &child).unwrap().commit().unwrap();
    // No fixture mappings or content accesses need retirement before removal.
    parent.remove_range(0, 2 * PAGE_SIZE_4K);
    assert!(parent.page_at(0).is_none());
    assert!(parent.page_at(PAGE_SIZE_4K as u64).is_none());
    assert!(child.page_at(0).unwrap().is_exclusive());
    assert!(!private.is_exclusive());
    drop(private);
    assert_last_hold_returns_to_inventory(private_pte);
    child.remove_range(0, PAGE_SIZE_4K);
    assert!(child.page_at(0).is_none());
    assert!(!shared.is_exclusive());
    drop(shared);
    assert_last_hold_returns_to_inventory(shared_pte);
}
