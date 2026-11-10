// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `dma_fence_chain`: an ordered timeline of fences (`dma-fence-chain.c`).
//!
//! A chain node records two dependencies at once: the work it contains and the
//! node of the same timeline before it. The node completes only when both did,
//! and reports the premise's failure first. This is how a timeline point
//! expresses "after everything this point replaces *and* after the work just
//! submitted" without waiting for either at construction time.
//!
//! Waiting walks the chain: a premise that already completed is skipped before
//! the task parks, and the node's premise is advanced as the walk passes the
//! completed links, so a long chain of finished work never leaves a waiter
//! parked on a node the timeline already passed.
//!
//! Chains stay short for the same reason Linux keeps them short: a chain node
//! is only linked onto another chain node, and the walk collapses the completed
//! links each time it runs.

use alloc::{sync::Arc, vec::Vec};

use super::{
    dma_fence::{DmaFence, FenceCallback, SyncContext},
    fence_list::FenceList,
};

/// Creates the chain node a replacement installs over `prev`.
///
/// The node takes the context and sequence number of the work it contains, so
/// one timeline reports one consistent `(context, seqno)` and a later walk can
/// tell how far the timeline advanced. The deadline the producer published for
/// the work is the one the node inherits (`dma_fence_set_deadline` walks a
/// chain to its contained work).
pub(crate) fn node(prev: Option<Arc<DmaFence>>, work: Arc<DmaFence>) -> Arc<DmaFence> {
    let (node, signal) = DmaFence::new_chain(
        prev,
        work.clone(),
        SyncContext::new(work.context().value()),
        work.seqno(),
    );
    if let Some(deadline) = work.deadline() {
        node.set_deadline(deadline);
    }
    // The node completes through its dependencies (`try_complete`); the
    // capability exists only so the node owns a completion point while they
    // are outstanding, and it is released with the node.
    drop(signal);
    node
}

/// Links `node` after `premise` and arranges its completion.
///
/// The node completes once `premise` completed and once its contained work
/// completed; the callbacks are registered here and never run before the
/// caller finished building the node.
pub(crate) fn link(node: &Arc<DmaFence>, premise: Option<&Arc<DmaFence>>) {
    if let Some(premise) = premise {
        premise.add_callback(FenceCallback::chain_premise(node));
    }
    if let Some(work) = node.contained()
        && !work.is_signaled()
    {
        work.add_callback(FenceCallback::chain_work(node));
    }
    try_complete(node);
}

/// Completes `node` if both of its dependencies reached a terminal state.
///
/// Called from either dependency's callback (which runs after that dependency
/// is terminal) and once after [`link`], so a chain built from completed work
/// completes immediately.
pub(crate) fn try_complete(node: &Arc<DmaFence>) {
    if node.is_signaled() || !node.is_chain() {
        return;
    }
    let Some(work) = node.contained() else {
        return;
    };
    // `walk` clears a satisfied premise. A missing premise means the chain has
    // no remaining prerequisite, not that the node is incomplete.
    let premise = node.chain_prev();
    if !work.is_signaled()
        || premise
            .as_ref()
            .is_some_and(|premise| !premise.is_signaled())
    {
        return;
    }
    let error = premise
        .as_ref()
        .filter(|premise| premise.error() != 0)
        .map_or_else(|| work.error(), |premise| premise.error());
    if error == 0 {
        node.signal();
    } else {
        node.signal_error(error);
    }
}

/// Advances `chain` past the premises that already completed.
///
/// Returns the first premise that is still pending, if any.
pub(crate) fn walk(chain: &Arc<DmaFence>) -> Option<Arc<DmaFence>> {
    let mut prev = chain.chain_prev();
    while let Some(candidate) = prev.clone() {
        if !candidate.is_signaled() {
            return Some(candidate);
        }
        let next = candidate.chain_prev();
        // Publish the skip: a concurrent waiter then starts from the pending
        // part of the chain instead of the completed one.
        chain.set_chain_prev(next.clone());
        prev = next;
    }
    None
}

/// The fences a wait on `chain` parks on.
///
/// A chain node completes once its premise and its contained work did, so a
/// waiter parks on those two instead of on the node: that is what a producer
/// updates, and it is where a `dma_fence_set_deadline` hint lands.
pub(crate) fn waiting_set(chain: &Arc<DmaFence>) -> Arc<FenceList> {
    let mut fences = Vec::new();
    if let Some(prev) = walk(chain) {
        fences.push(prev);
    }
    if let Some(work) = chain.contained()
        && !work.is_signaled()
    {
        fences.push(work);
    }
    if fences.is_empty() {
        // Nothing is pending, so the node's own state is the answer.
        fences.push(chain.clone());
    }
    FenceList::new(fences)
}
