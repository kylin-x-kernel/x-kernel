// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Downward-only radix storage. Published nodes have immutable heights.

use alloc::boxed::Box;
use core::{
    mem::offset_of,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use krcu::{CallbackNode, PreparedCallback};

pub(super) const RADIX_BITS: u32 = 6;
pub(super) const FANOUT: usize = 1 << RADIX_BITS;
pub(super) const MAX_HEIGHT: u32 = usize::BITS.div_ceil(RADIX_BITS);

pub(super) struct Node {
    pub(super) height: u32,
    pub(super) slots: [AtomicPtr<()>; FANOUT],
    // Atomic storage permits shared node references, not independent updates:
    // writers serialize each slot change and its count as one transaction.
    occupied: AtomicUsize,
    callback: CallbackNode,
}

impl Node {
    pub(super) fn empty() -> Self {
        Self {
            height: 0,
            slots: [const { AtomicPtr::new(core::ptr::null_mut()) }; FANOUT],
            occupied: AtomicUsize::new(0),
            callback: CallbackNode::new(release),
        }
    }

    pub(super) fn covers(&self, index: usize) -> bool {
        self.height == MAX_HEIGHT || (index >> (self.height * RADIX_BITS)) == 0
    }

    pub(super) fn slot(&self, index: usize) -> &AtomicPtr<()> {
        &self.slots[(index >> ((self.height - 1) * RADIX_BITS)) & (FANOUT - 1)]
    }

    // Caller holds writer exclusion or owns an unpublished node. Slot is empty.
    pub(super) fn insert_slot(&self, index: usize, value: *mut ()) {
        debug_assert!(self.slot(index).load(Ordering::Relaxed).is_null());
        self.slot(index).store(value, Ordering::Release);
        self.occupied
            .store(self.occupied.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    }

    // Caller holds writer exclusion; each nonempty slot owns exactly one entry.
    pub(super) fn remove_slot(&self, index: usize) -> *mut () {
        let old = self
            .slot(index)
            .swap(core::ptr::null_mut(), Ordering::AcqRel);
        debug_assert!(!old.is_null());
        self.occupied
            .store(self.occupied.load(Ordering::Relaxed) - 1, Ordering::Relaxed);
        old
    }

    pub(super) fn is_empty(&self) -> bool {
        self.occupied.load(Ordering::Relaxed) == 0
    }

    /// # Safety
    /// `raw` transfers one empty node detached under writer exclusion. Preflight
    /// succeeded; submit unlocked in the same admitted task context. No writer
    /// may reuse the node, and its callback has never been submitted before.
    pub(super) unsafe fn retire(raw: *mut Self) {
        // SAFETY: field projection retains the enclosing allocation provenance;
        // it does not create an exclusive reference while old readers exist.
        let callback = unsafe { &raw mut (*raw).callback };
        // SAFETY: the field of a live node allocation cannot be null.
        let callback = unsafe { NonNull::new_unchecked(callback) };
        // SAFETY: stable unique callback, raw node retained until grace period;
        // rejected submission deliberately retains storage still used by readers.
        let prepared = unsafe { PreparedCallback::new(callback, retain_rejected) };
        // SAFETY: caller supplies withdrawal, preflight and submission context.
        let is_accepted = unsafe { prepared.submit() }.is_ok();
        // As with payload retirement, rejection after withdrawal violates the
        // admitted context/permanent-service contract. Cancellation retains this
        // node: reporting the violation must not free storage readers can use.
        assert!(is_accepted, "admitted XArray node retirement was rejected");
    }
}

/// # Safety
/// The pointer is the callback field of a rejected detached node. The node must
/// remain allocated: no completed grace period protects its destruction yet.
unsafe fn retain_rejected(_callback: *mut CallbackNode) {}

/// # Safety
/// `callback` is the raw field projection of a detached empty Node, invoked once
/// after its grace period. No executor accesses this callback after invocation.
unsafe fn release(callback: *mut CallbackNode) {
    // SAFETY: recover the allocation from its actual field offset and provenance.
    let raw = unsafe { callback.byte_sub(offset_of!(Node, callback)) }.cast::<Node>();
    // SAFETY: all old readers ended; this callback owns exactly this allocation.
    // Shallow Drop never follows child or payload pointers.
    drop(unsafe { Box::from_raw(raw) });
}

pub(super) fn height_for(index: usize) -> u32 {
    (usize::BITS - index.leading_zeros())
        .max(1)
        .div_ceil(RADIX_BITS)
}

/// Owns only unpublished allocations, never a borrowed part of the live tree.
pub(super) struct PreparedNodes {
    nodes: [Option<Box<Node>>; 2 * MAX_HEIGHT as usize],
    pub(super) len: usize,
}

impl PreparedNodes {
    pub(super) fn new() -> Self {
        Self {
            nodes: [const { None }; 2 * MAX_HEIGHT as usize],
            len: 0,
        }
    }

    pub(super) fn prepare(&mut self, needed: usize) {
        while self.len < needed {
            self.nodes[self.len] = Some(Box::new(Node::empty()));
            self.len += 1;
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: writer serialization preserves the \
                  prepared capacity calculated by nodes_needed"
    )]
    pub(super) fn take(&mut self, height: u32) -> *mut Node {
        self.len -= 1;
        let mut node = self.nodes[self.len]
            .take()
            .expect("prepared radix path capacity");
        node.height = height;
        Box::into_raw(node)
    }
}

#[cfg(test)]
#[path = "../tests/host/pruning.rs"]
mod tests_pruning;
