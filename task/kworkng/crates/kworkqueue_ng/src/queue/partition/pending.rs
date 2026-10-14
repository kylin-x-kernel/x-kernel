// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Non-owning FIFO linkage for prepared pending Work storage.
//!
//! Work embeds one node; partition state protection serializes list membership.
//! This layer neither owns Work nor decides lifecycle eligibility.

use alloc::sync::Arc;
use core::{marker::PhantomPinned, ptr::NonNull};

use intrusive_collections::{LinkedList, UnsafeRef, intrusive_adapter, linked_list::AtomicLink};

use super::QueuePartition;
use crate::work::execution::Execution;

/// Concrete-type claim entry selected once when Work is prepared.
pub(crate) type ClaimPending =
    unsafe fn(NonNull<PendingNode>, &Arc<QueuePartition>) -> Option<Execution>;

/// One stable intrusive link, with no duplicated owner, color or lifecycle flags.
pub(crate) struct PendingNode {
    link: AtomicLink,
    claim: ClaimPending,
    release: unsafe fn(NonNull<PendingNode>, &Arc<QueuePartition>) -> bool,
    _pin: PhantomPinned,
}

impl PendingNode {
    pub(crate) const fn with_claim(
        claim: ClaimPending,
        release: unsafe fn(NonNull<PendingNode>, &Arc<QueuePartition>) -> bool,
    ) -> Self {
        Self {
            link: AtomicLink::new(),
            claim,
            release,
            _pin: PhantomPinned,
        }
    }

    #[cfg(unittest)]
    pub(super) const fn new() -> Self {
        Self::with_claim(|_, _| None, |_, _| false)
    }

    /// Invokes only the concrete Work claim transaction, never its business callback.
    ///
    /// # Safety
    /// node must come from a live Work's pending_ptr with its original claim entry.
    /// Hold the owning partition lock through ready selection and unlink.
    /// Storage must outlive returned execution, which must be consumed exactly once.
    pub(super) unsafe fn claim(
        node: NonNull<Self>,
        partition: &Arc<QueuePartition>,
    ) -> Option<Execution> {
        // SAFETY: the caller retains initialized node storage during inspection.
        let claim = unsafe { node.as_ref() }.claim;
        // SAFETY: the caller supplies the matching concrete Work pointer and
        // owning partition protection, and preserves lifetime through execution.
        unsafe { claim(node, partition) }
    }

    /// Reports linkage, not which list owns it or whether Work is executable.
    pub(super) fn is_linked(&self) -> bool {
        self.link.is_linked()
    }

    /// Marks an eligible waiting occurrence ready before its protected list transfer.
    ///
    /// # Safety
    /// Keep the matching Work live/shared and hold its owning partition guard
    /// through the waiting-to-ready transfer and allowance charge. This entry
    /// does not create execution or invoke the business callback.
    pub(super) unsafe fn release(node: NonNull<Self>, partition: &Arc<QueuePartition>) -> bool {
        // SAFETY: caller retains initialized node storage under partition protection.
        let release = unsafe { node.as_ref() }.release;
        // SAFETY: immutable constructor pairing preserves concrete type/lifetime.
        unsafe { release(node, partition) }
    }
}

intrusive_adapter!(PendingAdapter = UnsafeRef<PendingNode>: PendingNode { link => AtomicLink });

/// FIFO membership without allocations, ownership retention or per-entry wrappers.
///
/// Push, ready claim and direct removal are O(1). Waiting release scans once
/// and transfers eligible nodes without moving skipped nodes.
/// Dropping a list only unlinks
/// nodes: it does not cancel Work, release its resources or complete its observers.
pub(super) struct PendingList {
    list: LinkedList<PendingAdapter>,
}

impl PendingList {
    pub(super) const fn new() -> Self {
        Self {
            list: LinkedList::new(PendingAdapter::NEW),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Unlinks the first ready node, preserving its original pointer provenance.
    pub(super) fn pop_front(&mut self) -> Option<NonNull<PendingNode>> {
        let mut cursor = self.list.front_mut();
        let node = cursor.get_ptr()?;
        cursor.remove();
        Some(node)
    }

    /// Transfers up to limit eligible nodes in one scan, preserving skipped order.
    ///
    /// The predicate is internal, non-reentrant management code. It marks Work
    /// ready under its lock; the caller holds the partition lock throughout.
    pub(super) fn transfer_matching(
        &mut self,
        target: &mut Self,
        limit: usize,
        mut release: impl FnMut(NonNull<PendingNode>) -> bool,
    ) -> usize {
        let mut cursor = self.list.front_mut();
        let mut released = 0;
        while released < limit {
            let Some(node) = cursor.get_ptr() else { break };
            if release(node) {
                target
                    .list
                    .push_back(cursor.remove().expect("cursor has a node"));
                released += 1;
            } else {
                cursor.move_next();
            }
        }
        released
    }

    /// Appends a prepared node; returns false unchanged if it is already linked.
    ///
    /// # Safety
    /// node must point to an aligned, initialized PendingNode. Keep its storage
    /// alive and immovable, without exclusive references to it,
    /// until removal/list drop and all borrowed list accesses have ended. Serialize
    /// operations on this node across all lists, including check and insertion.
    /// Pinning does not itself prevent premature destruction of external storage.
    pub(super) unsafe fn push_back(&mut self, node: NonNull<PendingNode>) -> bool {
        // SAFETY: insertion's contract supplies live initialized shared storage.
        if unsafe { node.as_ref() }.is_linked() {
            return false;
        }
        // SAFETY: the caller keeps this initialized shared node stable and live
        // through membership and list accesses, and excludes other link mutation.
        let reference = unsafe { UnsafeRef::from_raw(node.as_ptr()) };
        self.list.push_back(reference);
        true
    }

    /// Removes this pending node without scanning, destruction or notification.
    ///
    /// # Safety
    /// If linked, the node must belong to THIS list. A linked bit alone cannot
    /// establish that fact. Keep node storage valid and exclude concurrent node
    /// operations; the partition's validated Work association must prove ownership.
    pub(super) unsafe fn remove(&mut self, node: &PendingNode) -> bool {
        if !node.is_linked() {
            return false;
        }
        // SAFETY: membership in this exact list is the caller's precondition;
        // exclusive list access and serialized node operations protect the cursor.
        unsafe { self.list.cursor_mut_from_ptr(node) }
            .remove()
            .is_some()
    }

    /// Unlinks the earliest eligible node without moving any skipped node.
    ///
    /// The predicate is an internal protected eligibility check, not a business
    /// callback. It must not sleep or reenter partition/list operations. The
    /// partition couples selection with Work claim under its protection.
    /// The returned address does not extend storage lifetime and is never owned.
    #[cfg(unittest)]
    pub(super) fn pop_first_matching(
        &mut self,
        mut is_eligible: impl FnMut(NonNull<PendingNode>) -> bool,
    ) -> Option<NonNull<PendingNode>> {
        let mut cursor = self.list.front_mut();
        while let Some(node) = cursor.get_ptr() {
            if is_eligible(node) {
                cursor.remove();
                return Some(node);
            }
            cursor.move_next();
        }
        None
    }
}

unittest::path_mod_test!(
    pending_unittest,
    "../../../ut-tests/queue_partition_pending_unittest.rs"
);
