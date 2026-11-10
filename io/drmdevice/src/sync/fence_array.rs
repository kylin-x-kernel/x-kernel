// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `dma_fence_array`: one fence that stands for a whole set.
//!
//! An array is the merge of several fences into one completion point: the head
//! completes once every member did, and reports the first failure of the set.
//! Completion is not polled. Each member still pending at construction receives
//! a [`FenceCallback::MemberDone`] pointing at the array, and the array's
//! completion check runs when the *last* member arrives, because a member's
//! callback runs after that member is already in a terminal state.
//!
//! An array never completes early. Linux fails a fence array whose error was
//! important as soon as one member fails; this driver's only arrays merge
//! client fences and display dependencies, where an early failure would free or
//! reuse memory another member may still be using. The failure is therefore
//! remembered and reported on the head when the whole set is done.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use super::dma_fence::{DmaFence, FenceCallback, FenceSignal, SyncContext};

/// A set of fences merged into one completion point.
pub(crate) struct FenceArray {
    /// Members, in the order they were given.
    members: Vec<Arc<DmaFence>>,
    /// The fence userspace observes for the whole set. The head owns the
    /// array strongly; this weak back-reference avoids a cycle.
    head: Weak<DmaFence>,
    /// The capability that completes the head.
    signal: Arc<FenceSignal>,
}

impl FenceArray {
    /// The fence that stands for `members`.
    ///
    /// An empty or single-fence set needs no array: Linux's
    /// `dma_fence_unwrap_array` keeps the fence itself, so the identity of a
    /// plain dependency survives wrapping.
    pub(crate) fn from_list(
        members: &[Arc<DmaFence>],
        context: SyncContext,
        seqno: u64,
    ) -> Arc<DmaFence> {
        match members {
            [] => DmaFence::new_signaled(context, seqno),
            [only] => only.clone(),
            members => Self::new(members.to_vec(), context, seqno).head(),
        }
    }

    /// Builds the array and registers the member completions that complete it.
    ///
    /// A member that already completed runs its callback immediately, so the
    /// array is complete as soon as its last member was signaled at
    /// construction.
    pub(crate) fn new(members: Vec<Arc<DmaFence>>, context: SyncContext, seqno: u64) -> Arc<Self> {
        let (head, signal) = DmaFence::new_owned(context, seqno);
        // The array owns the only completion capability of its head for the
        // array's whole life, so completing the head never runs through a
        // capability whose other owner is the head itself.
        let array = Arc::new(Self {
            members,
            head: Arc::downgrade(&head),
            signal,
        });
        head.set_array(&array);
        for member in &array.members {
            member.add_callback(FenceCallback::member_done(&array));
        }
        array
    }

    /// Completes the head if every member reached a terminal state.
    ///
    /// Returns whether this call completed the array. The head reports the
    /// first member failure, if any member failed.
    pub(crate) fn try_signal(&self) -> bool {
        let Some(head) = self.head.upgrade() else {
            return false;
        };
        if !self.is_complete() || head.is_signaled() {
            return false;
        }
        let error = self.members.iter().find_map(|member| match member.error() {
            0 => None,
            error => Some(error),
        });
        // The head completes when its latest member did, and reports that
        // member's instant: a merge of completed work is as old as its premise.
        if let Some(stamp) = self
            .members
            .iter()
            .filter_map(|member| member.timestamp())
            .max_by_key(|stamp| stamp.nanos(khal::time::monotonic_time()))
        {
            head.inherit_timestamp(stamp);
        }
        let ran = match error {
            Some(error) => self.signal.signal_error(error),
            None => self.signal.signal(),
        };
        ran != 0
    }

    /// Whether every member reached a terminal state.
    pub(crate) fn is_complete(&self) -> bool {
        self.members.iter().all(|member| member.is_signaled())
    }

    /// The members.
    pub(crate) fn members(&self) -> &[Arc<DmaFence>] {
        &self.members
    }

    /// The head of the array.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the array is only exposed through its \
                  head, so the head is alive while this accessor can be called"
    )]
    pub(crate) fn head(&self) -> Arc<DmaFence> {
        self.head
            .upgrade()
            .expect("fence array head lives while the array is published")
    }
}

impl core::fmt::Debug for FenceArray {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FenceArray")
            .field("head", &self.head)
            .field("members", &self.members)
            .finish()
    }
}
