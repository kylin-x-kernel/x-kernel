// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::Ordering;

use unittest::def_test;

use super::{MAX_REFERENCES, ReferenceState};
use crate::{PageMeta, PageMetaError, UsageKind};

#[def_test]
fn reference_retirement_separates_last_release_from_inventory_reuse() {
    let meta = PageMeta::new();
    // SAFETY: this private fixture models an exclusive, unpublished page;
    // no allocator or hardware can access or reclaim its modeled storage.
    unsafe { meta.claim_reference(UsageKind::Global) }.unwrap();
    // SAFETY: the fixture retains its initial live hold during cloning.
    unsafe { meta.clone_reference() }.unwrap();
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced { references: 2 }
    );
    // SAFETY: consumes the extra hold once; the fixture has no hardware use.
    assert!(!unsafe { meta.release_reference() });
    // SAFETY: consumes the remaining initial hold once after all modeled use.
    assert!(unsafe { meta.release_reference() });
    assert_eq!(meta.reference_state(), ReferenceState::Retiring);
    // SAFETY: this fixture is the unique last holder selected immediately above.
    assert_eq!(unsafe { meta.finish_retirement() }, UsageKind::Global);
    assert_eq!(meta.reference_state(), ReferenceState::Unreferenced);
    // SAFETY: models reallocation only after retirement has fully completed.
    unsafe { meta.claim_reference(UsageKind::PageTable) }.unwrap();
    // SAFETY: consumes this new allocation's only hold, with no hardware use.
    assert!(unsafe { meta.release_reference() });
    // SAFETY: this fixture again owns the sole retirement obligation.
    assert_eq!(unsafe { meta.finish_retirement() }, UsageKind::PageTable);
}

#[def_test]
fn reference_limit_does_not_wrap_or_destroy_the_existing_hold() {
    let meta = PageMeta::new();
    // The isolated fixture models the limit without constructing billions of
    // holders. Restore its sole real hold before completing retirement.
    meta.references.store(MAX_REFERENCES, Ordering::Relaxed);
    // SAFETY: the modeled limit includes an existing hold retained by this test.
    let result = unsafe { meta.clone_reference() };
    assert_eq!(result, Err(PageMetaError::ReferenceOverflow));
    assert_eq!(
        meta.reference_state(),
        ReferenceState::Referenced {
            references: MAX_REFERENCES
        }
    );
    meta.references.store(1, Ordering::Relaxed);
    // SAFETY: consumes the fixture's sole real hold after all modeled use ends.
    assert!(unsafe { meta.release_reference() });
    // SAFETY: the true result selected this fixture as the unique last holder.
    unsafe { meta.finish_retirement() };
}

#[def_test]
fn permanent_metadata_cannot_be_claimed_as_an_allocation() {
    let mut meta = PageMeta::new();
    meta.mark_permanent();
    // SAFETY: this negative fixture has exclusive modeled storage and no raw
    // users; claim_reference must reject the reservation without changing it.
    let result = unsafe { meta.claim_reference(UsageKind::Global) };
    assert_eq!(result, Err(PageMetaError::InvalidState));
    assert!(meta.is_permanent());
    assert_eq!(meta.references.load(Ordering::Relaxed), 0);
    assert_eq!(meta.reference_state(), ReferenceState::Permanent);
}

#[def_test]
fn memory_cgroup_ownership_is_stamped_and_taken_once() {
    let meta = PageMeta::new();
    assert_eq!(meta.memcg(), 0);

    meta.set_memcg(7);
    assert_eq!(meta.memcg(), 7);
    assert_eq!(meta.take_memcg(), 7);
    assert_eq!(meta.memcg(), 0);
    assert_eq!(meta.take_memcg(), 0);
}
