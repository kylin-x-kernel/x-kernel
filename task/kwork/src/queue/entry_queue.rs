// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::WorkEntry;
use crate::{BarrierAttachResult, ScheduledWork, WorkBarrier};
#[cfg(unittest)]
use crate::{QueueOwner, WorkColor, WorkInstanceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingBarrierAttach {
    Attached,
    Missing,
    Full,
}

/// Maximum number of pending work entries one worker pool's ring holds.
///
/// The ring stays bounded and preallocated so enqueue remains IRQ-safe without
/// allocating in `queue_work()`. The capacity is the build-time
/// `WORKQUEUE_PENDING_CAP` configuration symbol; all logical workqueues
/// attached to one pool share this ring, so the effective per-queue capacity
/// depends on how many queues attach to the pool.
pub const MAX_WORKQUEUE_PENDING: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

/// Fixed-capacity queue-entry ring.
///
/// This is the shared storage primitive for queue-owned pending entries and
/// the pool/binding bookkeeping that needs a bounded, IRQ-safe container. Logical
/// workqueue accounting, active throttling, and flush colors belong to
/// `WorkQueuePoolState`, not to this ring.
pub(crate) struct WorkEntryQueue {
    entries: [Option<WorkEntry>; MAX_WORKQUEUE_PENDING],
    head: usize,
    len: usize,
}

/// Pending lane from which an entry was removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkEntryLane {
    /// Entry came from the pool runnable list and participates in active count.
    Runnable,
    /// Entry came from the binding inactive list and is throttled by max_active.
    Inactive,
}

/// Entry removed from one of the pending storage lanes.
pub(crate) struct PendingWorkEntry {
    entry: WorkEntry,
    lane: WorkEntryLane,
}

/// Linux-like pending storage for one worker pool.
///
/// `runnable` is the X-Kernel counterpart of Linux `worker_pool::worklist`.
/// `inactive` is the counterpart of `pool_workqueue::inactive_works`; entries
/// in it are pending but do not consume or provide runnable work until a binding
/// active slot becomes available. Both rings are fixed-capacity and the store
/// enforces one shared total capacity so IRQ-safe enqueue keeps the previous
/// bounded-allocation property.
pub(crate) struct PendingWorkStore {
    runnable: WorkEntryQueue,
    inactive: WorkEntryQueue,
}

impl WorkEntryQueue {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_WORKQUEUE_PENDING],
            head: 0,
            len: 0,
        }
    }

    fn physical_index(&self, logical_index: usize) -> usize {
        (self.head + logical_index) % MAX_WORKQUEUE_PENDING
    }

    fn push_entry(&mut self, entry: WorkEntry) -> Result<(), ()> {
        if self.len == MAX_WORKQUEUE_PENDING {
            return Err(());
        }
        let tail = self.physical_index(self.len);
        self.entries[tail] = Some(entry);
        self.len += 1;
        Ok(())
    }

    fn remove_logical_entry(&mut self, logical_index: usize) -> Option<WorkEntry> {
        let physical_index = self.physical_index(logical_index);
        let removed = self.entries[physical_index].take();

        for logical in logical_index..(self.len - 1) {
            let current = self.physical_index(logical);
            let next = self.physical_index(logical + 1);
            self.entries[current] = self.entries[next].take();
        }
        let tail = self.physical_index(self.len - 1);
        self.entries[tail] = None;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        removed
    }

    fn pop_front_entry(&mut self) -> Option<WorkEntry> {
        if self.len == 0 {
            return None;
        }
        let physical_index = self.head;
        let entry = self.entries[physical_index].take();
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        } else {
            self.head = self.physical_index(1);
        }
        entry
    }

    fn remove_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
    ) -> Option<WorkEntry> {
        let logical_index = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|queued| {
                    queued.work().same_work(work) && queued.binding_key() == binding_key
                })
        })?;
        self.remove_logical_entry(logical_index)
    }

    fn attach_barrier_to_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        let Some(logical_index) = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|entry| {
                    entry.work().same_work(work) && entry.binding_key() == binding_key
                })
        }) else {
            return PendingBarrierAttach::Missing;
        };
        let physical = self.physical_index(logical_index);
        let entry = self.entries[physical]
            .as_mut()
            .expect("found queued work entry should still exist");
        match entry.attach_barrier(barrier) {
            BarrierAttachResult::Attached => PendingBarrierAttach::Attached,
            BarrierAttachResult::Full => PendingBarrierAttach::Full,
        }
    }

    fn remove_first_for_binding(&mut self, binding_key: usize) -> Option<WorkEntry> {
        let logical_index = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|entry| entry.binding_key() == binding_key)
        })?;
        self.remove_logical_entry(logical_index)
    }

    fn pop_front_matching(&mut self, binding_key: usize) -> Option<WorkEntry> {
        let logical_index = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|entry| entry.binding_key() == binding_key)
        })?;
        self.remove_logical_entry(logical_index)
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(unittest)]
    fn count_for_binding(&self, binding_key: usize) -> usize {
        (0..self.len)
            .filter(|index| {
                self.entries[self.physical_index(*index)]
                    .as_ref()
                    .is_some_and(|entry| entry.binding_key() == binding_key)
            })
            .count()
    }
}

impl PendingWorkEntry {
    fn new(entry: WorkEntry, lane: WorkEntryLane) -> Self {
        Self { entry, lane }
    }

    pub(crate) fn entry(&self) -> &WorkEntry {
        &self.entry
    }

    pub(crate) fn entry_mut(&mut self) -> &mut WorkEntry {
        &mut self.entry
    }

    pub(crate) fn is_runnable(&self) -> bool {
        self.lane == WorkEntryLane::Runnable
    }

    pub(crate) fn into_work(self) -> ScheduledWork {
        self.entry.into_work()
    }
}

impl PendingWorkStore {
    pub(crate) const fn new() -> Self {
        Self {
            runnable: WorkEntryQueue::new(),
            inactive: WorkEntryQueue::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.runnable.len() + self.inactive.len()
    }

    fn push_entry(&mut self, entry: WorkEntry, lane: WorkEntryLane) -> Result<(), ()> {
        if self.len() == MAX_WORKQUEUE_PENDING {
            return Err(());
        }
        match lane {
            WorkEntryLane::Runnable => self.runnable.push_entry(entry),
            WorkEntryLane::Inactive => self.inactive.push_entry(entry),
        }
    }

    pub(crate) fn push_runnable(&mut self, entry: WorkEntry) -> Result<(), ()> {
        self.push_entry(entry, WorkEntryLane::Runnable)
    }

    #[cfg(unittest)]
    pub(crate) fn push(
        &mut self,
        work: &ScheduledWork,
        owner: QueueOwner,
        color: WorkColor,
    ) -> Result<(), ()> {
        self.push_runnable(WorkEntry::new(
            work.clone(),
            owner,
            color,
            WorkInstanceId::for_tests(1),
        ))
    }

    pub(crate) fn push_inactive(&mut self, entry: WorkEntry) -> Result<(), ()> {
        self.push_entry(entry, WorkEntryLane::Inactive)
    }

    #[cfg(unittest)]
    pub(crate) fn pending_len_for_binding(&self, binding_key: usize) -> usize {
        self.runnable.count_for_binding(binding_key) + self.inactive.count_for_binding(binding_key)
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_binding(&self, binding_key: usize) -> usize {
        self.runnable.count_for_binding(binding_key)
    }

    pub(crate) fn deactivate_runnable_for_binding(&mut self, binding_key: usize) -> usize {
        let mut moved = 0usize;
        while let Some(entry) = self.runnable.pop_front_matching(binding_key) {
            self.inactive
                .push_entry(entry)
                .expect("moving within pending store should keep total capacity");
            moved += 1;
        }
        moved
    }

    pub(crate) fn activate_first_inactive_for_binding(&mut self, binding_key: usize) -> bool {
        let Some(entry) = self.inactive.remove_first_for_binding(binding_key) else {
            return false;
        };
        self.runnable
            .push_entry(entry)
            .expect("moving within pending store should keep total capacity");
        true
    }

    pub(crate) fn remove_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
    ) -> Option<PendingWorkEntry> {
        if let Some(entry) = self.runnable.remove_work_for_key(work, binding_key) {
            return Some(PendingWorkEntry::new(entry, WorkEntryLane::Runnable));
        }
        self.inactive
            .remove_work_for_key(work, binding_key)
            .map(|entry| PendingWorkEntry::new(entry, WorkEntryLane::Inactive))
    }

    pub(crate) fn attach_barrier_to_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        match self
            .runnable
            .attach_barrier_to_work_for_key(work, binding_key, barrier.clone())
        {
            PendingBarrierAttach::Attached => PendingBarrierAttach::Attached,
            PendingBarrierAttach::Full => PendingBarrierAttach::Full,
            PendingBarrierAttach::Missing => {
                self.inactive
                    .attach_barrier_to_work_for_key(work, binding_key, barrier)
            }
        }
    }

    pub(crate) fn pop_runnable_candidate(&mut self) -> Option<WorkEntry> {
        self.runnable.pop_front_entry()
    }
}
