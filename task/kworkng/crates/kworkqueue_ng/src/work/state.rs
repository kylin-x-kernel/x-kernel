// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inline pending/running storage, not a public submission or execution handle.
//!
//! Integration must protect these fields with partition-then-Work locking and
//! detach cancellation resources and observations in that protected transaction.
//! Resource destruction and detached observer completion happen unlocked. The
//! generic resource payload lets tests exercise destruction before Queue exists;
//! it represents owned bookkeeping, not ownership of the enclosing Work.

use alloc::{sync::Arc, vec::Vec};

use super::observers::{CompletionObservers, PreparedObserver};
use crate::queue::partition::QueuePartition;

mod delayed;

/// One inline occurrence, including ownership and optional ordinary flush accounting.
pub(crate) struct WorkSlot<T> {
    pub(crate) resources: T,
    pub(crate) partition: Option<Arc<QueuePartition>>,
    // None before timer-to-queue transfer: ownership alone is not in-flight work.
    color: Option<usize>,
    pub(crate) observers: CompletionObservers,
}

impl<T> WorkSlot<T> {
    /// Consume detached resources, then retire the original accounting and observers.
    /// Call unlocked after the final Work access. Running callers must already
    /// have released their payload before clearing running/returning allowance.
    pub(super) fn complete(self) {
        let Self {
            resources,
            partition,
            color,
            observers,
        } = self;
        drop(resources);
        if let Some(partition) = partition {
            let notifications = color.map(|color| partition.retire_after_cleanup(color));
            drop(partition);
            if let Some(notifications) = notifications {
                notifications.complete();
            }
        }
        observers.complete();
    }
}

/// Mutually exclusive locations of the one accepted pending occurrence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingPlacement {
    Timer { deadline_ns: u64 },
    Waiting,
    Ready,
}

/// Pending placement distinguishes timer-only lifetime from queue allowance.
struct PendingSlot<T> {
    slot: WorkSlot<T>,
    placement: PendingPlacement,
}

/// Lock-protected Work data: one pending occurrence and one execution including cleanup.
pub(crate) struct WorkState<T> {
    disable_depth: usize,
    pending: Option<PendingSlot<T>>,
    // None resources means cleanup has begun, not that execution has retired.
    running: Option<WorkSlot<Option<T>>>,
}

impl<T> WorkState<T> {
    /// Counts current subjects requiring one prepared receipt each.
    pub(crate) fn observer_count(&self) -> usize {
        usize::from(self.pending.is_some()) + usize::from(self.running.is_some())
    }

    /// Attaches to every occupied slot after the owner validated capacity.
    ///
    /// # Panics
    /// Panics if prepared receipt capacity is smaller than observer_count.
    pub(crate) fn capture(&mut self, prepared: &mut Vec<PreparedObserver>) {
        if let Some(pending) = self.pending.as_mut() {
            pending
                .slot
                .observers
                .attach(prepared.pop().expect("validated observer capacity"));
        }
        if let Some(running) = self.running.as_mut() {
            running
                .observers
                .attach(prepared.pop().expect("validated observer capacity"));
        }
    }

    pub(super) fn new() -> Self {
        Self {
            disable_depth: 0,
            pending: None,
            running: None,
        }
    }

    /// Stores prepared resources without allocation or destruction.
    ///
    /// # Errors
    /// Returns the unchanged resources if disabled or pending storage is occupied.
    #[cfg(unittest)]
    pub(super) fn accept(&mut self, resources: T) -> Result<(), T> {
        if self.is_disabled() || self.pending.is_some() {
            return Err(resources);
        }
        self.pending = Some(PendingSlot {
            slot: WorkSlot {
                resources,
                partition: None,
                color: None,
                observers: CompletionObservers::new(),
            },
            placement: PendingPlacement::Ready,
        });
        Ok(())
    }

    /// Checks only Work occupancy and ownership, without touching partition state.
    pub(crate) fn can_accept_on(&self, partition: &Arc<QueuePartition>) -> bool {
        !self.is_disabled()
            && self.pending.is_none()
            && self.running.as_ref().is_none_or(|slot| {
                slot.partition
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, partition))
            })
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disable_depth != 0
    }

    /// Acquires one independent disable before unlocked pending cancellation.
    ///
    /// # Panics
    /// Panics before changing state if disable depth cannot be represented.
    pub(super) fn begin_disable(&mut self) {
        self.disable_depth = self
            .disable_depth
            .checked_add(1)
            .expect("disable depth overflow");
    }

    /// Releases exactly one held disable, without restoring canceled work.
    ///
    /// # Panics
    /// Panics if internal code releases a disable it does not own.
    pub(super) fn end_disable(&mut self) {
        self.disable_depth = self
            .disable_depth
            .checked_sub(1)
            .expect("unowned disable release");
    }

    /// Fills the empty pending slot after protected validation and accounting.
    ///
    /// The partition caller keeps Work protection from can_accept_on through here.
    /// This does not replace an occupied slot or change queue list membership.
    ///
    /// # Panics
    /// Panics if the caller did not preserve the validated admission state.
    pub(crate) fn set_pending(
        &mut self,
        resources: T,
        partition: &Arc<QueuePartition>,
        color: usize,
    ) {
        assert!(self.can_accept_on(partition), "admission remains protected");
        self.pending = Some(PendingSlot {
            slot: WorkSlot {
                resources,
                partition: Some(partition.clone()),
                color: Some(color),
                observers: CompletionObservers::new(),
            },
            placement: PendingPlacement::Waiting,
        });
    }

    /// Reserves this waiting occurrence for execution once its predecessor retires.
    /// The partition owns allowance and couples this change with list transfer.
    pub(crate) fn release_pending(&mut self) -> bool {
        if self.running.is_some() {
            return false;
        }
        let Some(pending) = self
            .pending
            .as_mut()
            .filter(|pending| pending.placement == PendingPlacement::Waiting)
        else {
            return false;
        };
        pending.placement = PendingPlacement::Ready;
        true
    }

    pub(crate) fn pending_is_ready(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.placement == PendingPlacement::Ready)
    }

    /// Moves the pending payload and whole observer chain into running storage.
    pub(crate) fn claim(&mut self) -> bool {
        if self.running.is_some() || !self.pending_is_ready() {
            return false;
        }
        let Some(pending) = self.pending.take() else {
            return false;
        };
        let pending = pending.slot;
        self.running = Some(WorkSlot {
            resources: Some(pending.resources),
            partition: pending.partition,
            color: pending.color,
            observers: pending.observers,
        });
        true
    }

    /// Detaches pending ownership for this cancellation invocation's cleanup.
    ///
    /// Release resources unlocked before completing existing observers. Later
    /// captures do not include this detached cleanup. Queue accounting must
    /// remain outstanding independently until resource release has completed.
    pub(crate) fn cancel_pending(&mut self) -> Option<WorkSlot<T>> {
        self.pending.take().map(|pending| pending.slot)
    }

    /// Moves resources out for unlocked cleanup while keeping running occupied.
    pub(super) fn take_running_resources(&mut self) -> Option<T> {
        self.running.as_mut()?.resources.take()
    }

    /// Detaches running observers after the caller has finished resource release.
    ///
    /// Returns None while resources are still in the slot or no execution exists.
    /// Taking resources alone does not prove their destruction: the integrating
    /// executor must actually release them before this protected terminal step.
    /// Complete returned observers only after the final borrowed Work access.
    pub(crate) fn finish_after_cleanup(&mut self) -> Option<WorkSlot<Option<T>>> {
        if self.running.as_ref()?.resources.is_some() {
            return None;
        }
        self.running.take()
    }

    pub(super) fn pending_partition(&self) -> Option<Arc<QueuePartition>> {
        self.pending.as_ref()?.slot.partition.clone()
    }

    /// Snapshots unfinished ownership, including a running occurrence's cleanup.
    pub(crate) fn owner_partition(&self) -> Option<Arc<QueuePartition>> {
        self.pending_partition()
            .or_else(|| self.running_partition())
    }

    /// Revalidates presence and identity, not just equal CPU IDs, before admission.
    pub(crate) fn owner_matches(&self, expected: Option<&Arc<QueuePartition>>) -> bool {
        let actual = self
            .pending
            .as_ref()
            .and_then(|p| p.slot.partition.as_ref())
            .or_else(|| self.running.as_ref().and_then(|r| r.partition.as_ref()));
        match (actual, expected) {
            (None, None) => true,
            (Some(actual), Some(expected)) => Arc::ptr_eq(actual, expected),
            _ => false,
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Borrows the accepted payload only while the caller holds Work protection.
    pub(crate) fn pending_resources(&self) -> Option<&T> {
        Some(&self.pending.as_ref()?.slot.resources)
    }

    pub(super) fn running_partition(&self) -> Option<Arc<QueuePartition>> {
        self.running.as_ref()?.partition.clone()
    }

    pub(crate) fn pending_on(&self, partition: &Arc<QueuePartition>) -> bool {
        self.pending
            .as_ref()
            .and_then(|pending| pending.slot.partition.as_ref())
            .is_some_and(|owner| Arc::ptr_eq(owner, partition))
    }

    /// Detects an admission racing a route lookup that observed no partition.
    pub(super) fn has_partitioned_pending(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.slot.partition.is_some())
    }

    pub(crate) fn running_on(&self, partition: &QueuePartition) -> bool {
        self.running
            .as_ref()
            .and_then(|slot| slot.partition.as_ref())
            .is_some_and(|owner| core::ptr::eq(owner.as_ref(), partition))
    }

    /// Captures pending completion without retaining a pointer to this slot.
    ///
    /// # Errors
    /// Returns the unchanged attachment token when no pending occurrence exists.
    #[cfg(unittest)]
    pub(super) fn observe_pending(
        &mut self,
        observer: PreparedObserver,
    ) -> Result<(), PreparedObserver> {
        let Some(pending) = self.pending.as_mut() else {
            return Err(observer);
        };
        pending.slot.observers.attach(observer);
        Ok(())
    }

    /// Captures execution completion, including when its cleanup is in progress.
    ///
    /// # Errors
    /// Returns the unchanged attachment token when no execution exists.
    #[cfg(unittest)]
    pub(super) fn observe_running(
        &mut self,
        observer: PreparedObserver,
    ) -> Result<(), PreparedObserver> {
        let Some(running) = self.running.as_mut() else {
            return Err(observer);
        };
        running.observers.attach(observer);
        Ok(())
    }
}

#[cfg(unittest)]
mod tests;
