// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable entry storage with a callback embedded in the same allocation.

use alloc::boxed::Box;
use core::{
    mem::offset_of,
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
};

use krcu::{CallbackNode, PreparedCallback};

pub(super) struct Node<T> {
    pub(super) next: AtomicPtr<Self>,
    pub(super) value: T,
    callback: CallbackNode,
}

impl<T: Send + Sync + 'static> Node<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            value,
            callback: CallbackNode::new(release::<T>),
        }
    }

    /// # Safety
    /// raw transfers the sole retirement right of an unlinked Node allocation.
    /// Preflight succeeded before withdrawal; submit unlocked in the same task
    /// without explicit context transitions. T's complete release is callback-safe.
    pub(super) unsafe fn retire(raw: *mut Self) {
        // SAFETY: raw is the original stable allocation pointer. Projection
        // retains provenance without creating an exclusive node reference.
        let callback = unsafe { &raw mut (*raw).callback };
        // SAFETY: a field within a live allocation cannot be null.
        let callback = unsafe { NonNull::new_unchecked(callback) };
        // SAFETY: this embedded callback has never been queued. Rejection keeps
        // its enclosing allocation alive; invocation consumes it only after GP.
        let prepared = unsafe { PreparedCallback::new(callback, retain_rejected) };
        // SAFETY: withdrawal, admission and callback context are caller duties.
        let is_accepted = unsafe { prepared.submit() }.is_ok();
        // Only a violated permanent-service/context invariant can reject here.
        // The cancellation path deliberately retains reader-visible storage.
        assert!(is_accepted, "admitted RCU list retirement was rejected");
    }
}

/// # Safety
/// The callback belongs to an unlinked allocation not covered by a completed GP.
/// Its storage must remain alive even when submission is unexpectedly rejected.
unsafe fn retain_rejected(_callback: *mut CallbackNode) {}

/// # Safety
/// callback is a field projection from one original Node<T> Box allocation,
/// detached and invoked exactly once after its GP. T's release is callback-safe.
unsafe fn release<T>(callback: *mut CallbackNode) {
    // SAFETY: subtract this concrete field offset within the original allocation,
    // retaining provenance regardless of the compiler's chosen field order.
    let raw = unsafe { callback.byte_sub(offset_of!(Node<T>, callback)) }.cast::<Node<T>>();
    // SAFETY: GP ended every old borrow; this callback owns the allocation.
    // Node Drop is shallow and never follows the possibly stale next pointer.
    drop(unsafe { Box::from_raw(raw) });
}

#[cfg(test)]
#[path = "../tests/host/retirement.rs"]
mod tests;
