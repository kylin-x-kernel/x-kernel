// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[test]
#[cfg(feature = "intrusive-collections")]
fn pointer_operations_transfer_one_count_without_storage() {
    use intrusive_collections::PointerOps;

    let drops = AtomicUsize::new(0);
    let owner = KArc::new_nowait(cases::Probe(&drops)).unwrap();
    let weak = KArc::downgrade(&owner);
    let operations = crate::KArcPointerOps::default();
    assert_eq!(core::mem::size_of_val(&operations), 0);
    let address = KArc::as_ptr(&owner);
    let pointer = operations.into_raw(owner);
    assert_eq!(pointer, address);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    // SAFETY: pointer is this operation's unrecovered transferred reference.
    let recovered = unsafe { operations.from_raw(pointer) };
    assert_eq!(KArc::strong_count(&recovered), 1);
    drop(recovered);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(weak.upgrade().is_none());
}

#[test]
fn raw_transfer_retains_one_strong_reference() {
    cases::raw_transfer_retains_one_strong_reference();
}

#[test]
fn independent_raw_transfers_preserve_extraction() {
    cases::independent_raw_transfers_preserve_extraction();
}

#[test]
fn raw_transfer_preserves_aligned_zst() {
    cases::raw_transfer_preserves_aligned_zst();
}

#[test]
#[cfg(feature = "intrusive-collections")]
fn list_rejection_and_removal_preserve_ownership() {
    cases::list_rejection_and_removal_preserve_ownership();
}

#[test]
#[cfg(feature = "intrusive-collections")]
fn independent_links_and_teardown_release_each_reference() {
    cases::independent_links_and_teardown_release_each_reference();
}

#[path = "../fixtures/karc.rs"]
#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: shared host fixtures must fail \
              on unexpected allocation failure or missing owners; the kernel-only mod_test macro \
              cannot register this host module"
)]
mod cases;

#[test]
#[cfg(feature = "intrusive-collections")]
fn library_take_transfers_owners_without_destruction() {
    cases::library_take_transfers_owners_without_destruction();
}

#[test]
fn shared_identity_and_counts() {
    cases::shared_identity_and_counts();
}

#[test]
fn weak_retains_storage_not_payload() {
    cases::weak_retains_storage_not_payload();
}

#[test]
fn empty_weak_cannot_upgrade() {
    cases::empty_weak_cannot_upgrade();
}

#[test]
fn extraction_invalidates_existing_weak() {
    cases::extraction_invalidates_existing_weak();
}

#[test]
fn failed_extraction_preserves_owner() {
    cases::failed_extraction_preserves_owner();
}

#[test]
fn reservation_failure_drops_input() {
    cases::reservation_failure_drops_input();
}

#[test]
fn invalid_flags_reject_input() {
    cases::invalid_flags_reject_input();
}

#[test]
fn zero_sized_values_still_need_shared_storage() {
    cases::zero_sized_values_still_need_shared_storage();
}

#[test]
fn aligned_value_preserves_alignment() {
    cases::aligned_value_preserves_alignment();
}

#[test]
fn shared_handles_are_send_and_sync_for_atomic_payloads() {
    cases::shared_handles_are_send_and_sync_for_atomic_payloads();
}

#[test]
fn concurrent_upgrade_and_last_drop() {
    extern crate std;

    let drops = AtomicUsize::new(0);
    let owned = KArc::new_nowait(cases::Probe(&drops)).unwrap();
    let weak = KArc::downgrade(&owned);
    let barrier = std::sync::Barrier::new(5);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let borrowed = &weak;
            let barrier = &barrier;
            scope.spawn(move || {
                let first = borrowed.upgrade().unwrap();
                barrier.wait();
                drop(first);
                for _ in 0..1000 {
                    drop(borrowed.upgrade());
                }
            });
        }
        barrier.wait();
        drop(owned);
    });
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn panicking_payload_does_not_resurrect() {
    extern crate std;

    struct PanickingProbe<'a>(&'a AtomicUsize);
    impl Drop for PanickingProbe<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
            panic!("payload unwind test");
        }
    }
    let drops = AtomicUsize::new(0);
    let owned = KArc::new_nowait(PanickingProbe(&drops)).unwrap();
    let weak = KArc::downgrade(&owned);
    let pointer = KArc::into_raw(owned);
    // SAFETY: pointer retains the single unclaimed reference to the probe.
    let owned = unsafe { KArc::from_raw(pointer) };
    assert!(std::panic::catch_unwind(|| drop(owned)).is_err());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(weak.upgrade().is_none());
    drop(weak);
}
