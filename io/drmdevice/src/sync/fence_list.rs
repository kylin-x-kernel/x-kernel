// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! An immutable list of fences, published as one atomically replaceable value.
//!
//! Every place that asks "which fences precede this work" — the pending signal
//! of a [`super::dma_fence::DmaFence`] (Linux's `cb_list`), a reservation's
//! write or read slots, a fence array’s members, a timeline point — shares the
//! same structure: the list is built once and then never changes, while a
//! *published* reference to the list is replaced under the owner's lock. The
//! list therefore needs no per-element mutation and no detachment: a waiter
//! that read a reference saw a complete, consistent set.
//!
//! The one process that is not immutable is the *caller* appending another
//! fence to a list it still owns exclusively, which is what
//! [`FenceList::push`] does; once the list is published it is never mutated
//! again.

use alloc::sync::Arc;

use super::dma_fence::DmaFence;

/// A published, append-completed list of fences.
#[derive(Default)]
pub(crate) struct FenceList {
    /// Members in the order they were added. A member may repeat: two owners
    /// waiting on the same fence must both be released by it.
    fences: alloc::vec::Vec<Arc<DmaFence>>,
}

impl FenceList {
    /// An empty list.
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self {
            fences: alloc::vec::Vec::new(),
        })
    }

    /// A list over `fences`.
    pub(crate) fn new(fences: alloc::vec::Vec<Arc<DmaFence>>) -> Arc<Self> {
        Arc::new(Self { fences })
    }

    /// The members.
    pub(crate) fn fences(&self) -> &[Arc<DmaFence>] {
        &self.fences
    }

    /// Appends one fence to a list this caller owns exclusively.
    ///
    /// Returns the same list; a published list must not be passed here.
    pub(crate) fn push(self: Arc<Self>, fence: Arc<DmaFence>) -> Arc<Self> {
        match Arc::try_unwrap(self) {
            Ok(mut list) => {
                list.fences.push(fence);
                Arc::new(list)
            }
            // The list is still shared, so appending cannot mutate it in place.
            // Copying keeps the invariant that a published list never changes;
            // the extra allocation is paid only when a caller raced a reader.
            Err(shared) => {
                let mut fences = shared.fences.clone();
                fences.push(fence);
                Arc::new(Self { fences })
            }
        }
    }
}

impl core::fmt::Debug for FenceList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.fences.iter()).finish()
    }
}

/// Whether every fence of `list` reached a terminal state.
///
/// This is a free function because callers usually hold the structure that
/// publishes the list and cannot borrow it again to ask.
pub(crate) fn is_signaled(list: &FenceList) -> bool {
    list.fences().iter().all(|fence| fence.is_signaled())
}

/// The first non-zero error among the completed fences of `list`, or zero.
///
/// A pending fence may already carry an error recorded by `dma_fence_set_error`
/// (a commit that failed before it could be completed); that value is not a
/// result until the fence signals, or a waiter would report a failure for work
/// that is still running.
pub(crate) fn first_error(list: &FenceList) -> i32 {
    list.fences()
        .iter()
        .find_map(|fence| {
            if !fence.is_signaled() {
                return None;
            }
            match fence.error() {
                0 => None,
                error => Some(error),
            }
        })
        .unwrap_or(0)
}
