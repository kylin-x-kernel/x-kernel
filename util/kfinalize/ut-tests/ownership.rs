// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};

use unittest::def_test;

#[def_test]
fn identity_allocator_clone_keeps_original_release_token() {
    use core::alloc::{Allocator, Layout};
    let layout = Layout::from_size_align(17, 4096).unwrap();
    let (allocation, copy) = {
        let allocator = super::service::identity_allocator();
        (allocator.allocate(layout).unwrap(), allocator.clone())
    };
    assert_eq!(allocation.cast::<u8>().as_ptr().addr() % 4096, 0);
    assert!(copy.allocate(layout).is_err());
    // SAFETY: a copied allocator owns the same original token and matching layout;
    // this is the only deallocation, in ordinary task context.
    unsafe { copy.deallocate(allocation.cast(), layout) };
}

#[def_test]
fn report_deferred_identity_layout() {
    unittest::ktest_println!(
        "deferred layout: identity_bytes={} identity_align={} handle_bytes={} weak_bytes={} \
         raw_header_bytes={} usize_payload_bytes={} zst_payload_bytes={}",
        core::mem::size_of::<super::Identity<usize>>(),
        core::mem::align_of::<super::Identity<usize>>(),
        core::mem::size_of::<super::DeferredArc<usize>>(),
        core::mem::size_of::<super::DeferredWeak<usize>>(),
        core::mem::size_of::<super::RawIdentity<usize>>(),
        core::mem::size_of::<super::Payload<usize>>(),
        core::mem::size_of::<super::Payload<()>>()
    );
}

use super::{
    CreateError, DeferredArc,
    service::{Finalization, Prepared},
};

struct Held {
    pending: UnsafeCell<Option<Finalization>>,
}

// SAFETY: the only shared cell access is the one-shot submit required by Prepared.
// Tests recover the token only through exclusive Arc::get_mut after submit returns.
unsafe impl Sync for Held {}

// SAFETY: submit stores rather than runs the token, without allocating. Tests
// explicitly resume it in the sleepable test task after all public owners end.
unsafe impl Prepared for Held {
    unsafe fn submit(self: Arc<Self>, finalization: Finalization) {
        // SAFETY: the caller submits once; no reader is allowed until Arc::get_mut
        // certifies that this publisher and every identity have released Held.
        unsafe { *self.pending.get() = Some(finalization) };
    }
}

#[derive(Debug)]
struct Payload {
    drops: Arc<AtomicUsize>,
    value: usize,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

/// Weak expiry precedes finalization, even with an explicitly paused executor.
///
/// # Panics
/// Panics on identity resurrection, early destruction or duplicate finalization.
#[def_test]
fn paused_finalization_does_not_extend_weak_lifetime() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut held = Arc::new(Held {
        pending: UnsafeCell::new(None),
    });
    let owner = DeferredArc::new_prepared(
        Payload {
            drops: drops.clone(),
            value: 42,
        },
        || Ok(held.clone()),
    )
    .unwrap();
    let weak = DeferredArc::downgrade(&owner);
    let weak_clone = weak.clone();
    let other = owner.clone();
    assert!(weak.ptr_eq(&weak_clone));
    assert_eq!(weak.strong_count(), 2);
    assert_eq!(other.as_ref().value, 42);
    assert_eq!(other.value, 42);
    drop(owner);
    assert_eq!(weak.upgrade().unwrap().value, 42);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(other);
    assert_eq!(weak.strong_count(), 0);
    assert!(weak.ptr_eq(&weak_clone));
    assert!(weak.upgrade().is_none());
    assert!(weak_clone.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Acquire), 0);
    let token = Arc::get_mut(&mut held)
        .unwrap()
        .pending
        .get_mut()
        .take()
        .unwrap();
    drop(weak);
    drop(weak_clone);
    // SAFETY: the sleepable test task resumes this unique held obligation after
    // every public strong borrow ended, outside submit and without any locks.
    unsafe { token.finish() };
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(Arc::get_mut(&mut held).unwrap().pending.get_mut().is_none());
}

#[def_test]
fn raw_owners_preserve_counts_and_defer_finalization() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut held = Arc::new(Held {
        pending: UnsafeCell::new(None),
    });
    let owner = DeferredArc::new_prepared(
        Payload {
            drops: drops.clone(),
            value: 91,
        },
        || Ok(held.clone()),
    )
    .unwrap();
    let weak = DeferredArc::downgrade(&owner);
    let borrowed_pointer = DeferredArc::as_ptr(&owner);
    let raw = DeferredArc::into_raw(owner.clone());
    assert_eq!(borrowed_pointer, raw);
    assert_eq!(DeferredArc::strong_count(&owner), 2);
    // SAFETY: raw represents the unconsumed cloned strong owner, with the
    // original allocation provenance returned by into_raw.
    let recovered = unsafe { DeferredArc::from_raw(raw) };
    assert!(DeferredArc::ptr_eq(&owner, &recovered));
    assert_eq!(DeferredArc::strong_count(&recovered), 2);
    drop(owner);
    assert_eq!(recovered.value, 91);
    assert_eq!(DeferredArc::strong_count(&recovered), 1);
    drop(recovered);
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Acquire), 0);
    let token = Arc::get_mut(&mut held)
        .unwrap()
        .pending
        .get_mut()
        .take()
        .unwrap();
    drop(weak);
    // SAFETY: the test resumes the unique obligation after all strong and raw
    // owners ended, in its ordinary task context with no retained payload borrows.
    unsafe { token.finish() };
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

/// Preparation errors preserve ownership of the unchanged input.
///
/// # Panics
/// Panics if either recoverable preparation error consumes or modifies the input.
#[def_test]
fn preparation_error_returns_original_payload() {
    for error in [CreateError::NotReady, CreateError::InvalidContext] {
        let drops = Arc::new(AtomicUsize::new(0));
        let result = DeferredArc::new_prepared(
            Payload {
                drops: drops.clone(),
                value: 73,
            },
            || Err(error),
        );
        let Err((actual, value)) = result else {
            panic!("preparation should fail")
        };
        assert_eq!(actual, error);
        assert_eq!(value.value, 73);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(value);
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }
}
