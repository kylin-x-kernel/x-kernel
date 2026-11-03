// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A detached owner stays raw until the callback's grace period has completed.

use core::{marker::PhantomData, mem::offset_of, ptr::NonNull};

use kalloc_types::KBox;
use krcu::{CallbackNode, PreparedCallback};

use crate::EntryOwner;

pub(super) struct Retirement<P: EntryOwner> {
    callback: CallbackNode,
    owner: *mut (),
    carrier: PhantomData<P>,
}

impl<P: EntryOwner> Retirement<P> {
    pub(super) fn prepare(allocator: &mut impl crate::Allocator) -> kerrno::KResult<KBox<Self>> {
        allocator.allocate(Self {
            callback: CallbackNode::new(release::<P>),
            owner: core::ptr::null_mut(),
            carrier: PhantomData,
        })
    }

    /// # Safety
    /// `owner` transfers one displaced raw P whose release chain is callback-safe.
    /// Preflight succeeded before withdrawal; submit from the same task without
    /// explicit context transitions, holding no writer/RCU/scheduler lock.
    pub(super) unsafe fn submit(mut record: KBox<Self>, owner: *mut ()) {
        record.owner = owner;
        let raw = KBox::into_raw(record);
        // SAFETY: projection from the allocation pointer retains provenance for
        // recovering the enclosing record, independent of Rust field ordering.
        let node = unsafe { &raw mut (*raw).callback };
        // SAFETY: a live KBox field cannot be null.
        let node = unsafe { NonNull::new_unchecked(node) };
        // SAFETY: unique stable storage, certified transferable ownership and
        // nonblocking callback. Cancellation releases only the record, never a
        // displaced payload still accessible by old readers.
        let callback = unsafe { PreparedCallback::new(node, cancel::<P>) };
        // SAFETY: caller supplies preflight, withdrawal and execution context.
        let is_accepted = match unsafe { callback.submit() } {
            Ok(()) => true,
            Err((_, callback)) => {
                drop(callback);
                false
            }
        };
        // Rejection after withdrawal violates permanent-service or caller
        // context invariants. Retain the raw owner rather than reclaim early
        // or falsely report an unchanged association as an ordinary error.
        assert!(is_accepted, "admitted XArray retirement was rejected");
    }
}

/// # Safety
/// `node` is the raw field projection of one unqueued Retirement<P> KBox.
/// Consume only its record; withdrawal does not prove payload readers ended.
unsafe fn cancel<P: EntryOwner>(node: *mut CallbackNode) {
    // SAFETY: subtract the actual field offset within the same KBox allocation.
    let raw = unsafe { node.byte_sub(offset_of!(Retirement<P>, callback)) }.cast::<Retirement<P>>();
    // SAFETY: unique rejected submission owns the record. Its owner field is
    // deliberately raw, so dropping the record cannot release protected data.
    drop(unsafe { KBox::from_raw(raw) });
}

/// # Safety
/// `node` is the raw field projection of one accepted Retirement<P> KBox, now
/// detached for its sole invocation after the protecting grace period. No
/// executor will access it again. Its complete payload release must not block.
unsafe fn release<P: EntryOwner>(node: *mut CallbackNode) {
    // SAFETY: recover the enclosing allocation using this concrete layout.
    let raw = unsafe { node.byte_sub(offset_of!(Retirement<P>, callback)) }.cast::<Retirement<P>>();
    // SAFETY: the callback has unique record ownership; readers borrow only the
    // separate payload allocation, whose ownership is not reconstructed yet.
    let record = unsafe { KBox::from_raw(raw) };
    // SAFETY: this grace period ended all readers of the displaced ownership.
    // Reconstruct and release that one P only now, independently of the array.
    unsafe { P::drop_raw(record.owner) };
}

#[cfg(test)]
#[path = "../tests/host/retirement.rs"]
mod tests;
