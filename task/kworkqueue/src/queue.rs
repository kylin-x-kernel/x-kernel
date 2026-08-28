// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Logical workqueue and per-CPU binding state.

use core::num::NonZeroUsize;

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;

use crate::{
    BindingId, EntryOwner, Work, WorkColor, WorkInstanceId,
    executor::{ExecutorEntry, ExecutorOp},
    id::PendingRecordId,
    pending::{PendingRecord, PendingRecordTable},
    work::{PendingWorkState, RunningWorkState, WorkStateKind},
};

/// Global workqueue object with one runtime binding per CPU/pool.
pub struct WorkQueue<const CPUS: usize, const PENDING_CAP: usize> {
    name: &'static str,
    max_active: usize,
    bindings: [SpinNoIrq<WorkQueueBindingState<PENDING_CAP>>; CPUS],
}

impl<const CPUS: usize, const PENDING_CAP: usize> WorkQueue<CPUS, PENDING_CAP> {
    pub const fn new(name: &'static str, max_active: usize) -> Self {
        Self {
            name,
            max_active,
            bindings: [const { SpinNoIrq::new(WorkQueueBindingState::new()) }; CPUS],
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub const fn max_active(&self) -> usize {
        self.max_active
    }

    pub fn binding(&self, cpu_id: LogicalCpuId) -> Option<WorkQueueBinding<'_, CPUS, PENDING_CAP>> {
        (cpu_id.as_usize() < CPUS).then_some(WorkQueueBinding {
            queue: self,
            cpu_id,
        })
    }
}

#[derive(Clone, Copy)]
pub struct WorkQueueBinding<'queue, const CPUS: usize, const PENDING_CAP: usize> {
    queue: &'queue WorkQueue<CPUS, PENDING_CAP>,
    cpu_id: LogicalCpuId,
}

impl<const CPUS: usize, const PENDING_CAP: usize> WorkQueueBinding<'_, CPUS, PENDING_CAP> {
    pub fn cpu_id(self) -> LogicalCpuId {
        self.cpu_id
    }

    pub fn owner(self) -> EntryOwner {
        let state = &self.queue.bindings[self.cpu_id.as_usize()];
        let raw = core::ptr::from_ref(state).addr();
        EntryOwner::new(NonZeroUsize::new(raw).expect("binding address is non-zero"))
    }

    pub fn binding_id(self) -> BindingId {
        let state = &self.queue.bindings[self.cpu_id.as_usize()];
        let raw = core::ptr::from_ref(state).addr();
        BindingId::new(NonZeroUsize::new(raw).expect("binding address is non-zero"))
    }

    pub const fn queue_name(self) -> &'static str {
        self.queue.name()
    }

    pub fn work_key_for_entry(self, entry: ExecutorEntry) -> Option<crate::WorkKey> {
        if entry.binding != self.binding_id() || entry.owner != self.owner() {
            return None;
        }
        let record_id = PendingRecordId::from_payload(entry.payload);
        self.queue.bindings[self.cpu_id.as_usize()]
            .lock()
            .pending
            .get(record_id)
            .map(|record| record.work)
    }

    pub fn queue_work(self, work: &Work) -> QueueWorkResult {
        let mut work_guard = work.state().lock();
        match work_guard.kind {
            WorkStateKind::Idle => {}
            WorkStateKind::Running(ref running) if running.owner == self.owner() => {
                if work_guard.disable_depth != 0 {
                    return Err(QueueWorkError::Disabled);
                }
                return self.queue_running_requeue_locked(work, &mut work_guard);
            }
            _ => return Err(QueueWorkError::AlreadyQueued),
        }
        if work_guard.disable_depth != 0 {
            return Err(QueueWorkError::Disabled);
        }
        self.queue_work_locked(work, &mut work_guard)
    }

    pub fn mark_delayed(self, work: &Work) -> Result<(), QueueWorkError> {
        let mut work_guard = work.state().lock();
        if !matches!(work_guard.kind, WorkStateKind::Idle) {
            return Err(QueueWorkError::AlreadyQueued);
        }
        if work_guard.disable_depth != 0 {
            return Err(QueueWorkError::Disabled);
        }
        work_guard.kind = WorkStateKind::DelayedPending {
            target_owner: self.owner(),
        };
        Ok(())
    }

    pub fn activate_delayed(self, work: &Work) -> QueueWorkResult {
        let mut work_guard = work.state().lock();
        match work_guard.kind {
            WorkStateKind::DelayedPending { target_owner } if target_owner == self.owner() => {
                if work_guard.disable_depth != 0 {
                    return Err(QueueWorkError::Disabled);
                }
                work_guard.kind = WorkStateKind::Idle;
                self.queue_work_locked(work, &mut work_guard)
            }
            WorkStateKind::Idle => Err(QueueWorkError::NotPendingDelayed),
            _ => Err(QueueWorkError::AlreadyQueued),
        }
    }

    fn queue_work_locked(
        self,
        work: &Work,
        work_guard: &mut crate::work::WorkState,
    ) -> QueueWorkResult {
        debug_assert!(matches!(work_guard.kind, WorkStateKind::Idle));
        let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        let instance = work_guard.alloc_instance();
        let active = binding.active < self.queue.max_active;
        let color = binding.current_color;
        let owner = self.owner();
        let key = instance.as_key();
        let record = PendingRecord {
            work: work.key(),
            owner,
            key,
            instance,
            color,
            active,
        };
        let record_id = binding
            .pending
            .insert(record)
            .map_err(|_| QueueWorkError::PendingFull)?;
        let entry = ExecutorEntry::new(self.binding_id(), owner, key, record_id.payload());

        if active {
            binding.active += 1;
        }
        binding.in_flight[color.index()] += 1;
        work_guard.kind = WorkStateKind::Pending(PendingWorkState {
            owner,
            record: record_id,
            instance,
        });

        Ok(if active {
            QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry))
        } else {
            QueueWorkOutcome::Inactive(ExecutorOp::EnqueueInactive(entry))
        })
    }

    pub fn claim(
        self,
        entry: ExecutorEntry,
        work: &Work,
        worker_id: usize,
        worker_token: usize,
    ) -> ClaimResult {
        if entry.binding != self.binding_id() || entry.owner != self.owner() {
            return ClaimResult::Stale;
        }
        let record_id = PendingRecordId::from_payload(entry.payload);
        let record = {
            let binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            match binding.pending.get(record_id) {
                Some(record) => record,
                None => return ClaimResult::Stale,
            }
        };

        if record.work != work.key() {
            self.discard_pending_record(record_id, record);
            return ClaimResult::Stale;
        }

        let mut work_guard = work.state().lock();
        match work_guard.kind {
            WorkStateKind::Pending(pending)
                if pending.owner == record.owner
                    && pending.record == record_id
                    && pending.instance == record.instance =>
            {
                let removed = {
                    let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
                    binding.pending.remove(record_id)
                };
                if removed.is_none() {
                    return ClaimResult::Stale;
                }
                work_guard.kind = WorkStateKind::Running(RunningWorkState {
                    owner: record.owner,
                    key: record.key,
                    instance: record.instance,
                    color: record.color,
                    active: record.active,
                    worker_id,
                    worker_token,
                    canceling: false,
                    requeue: None,
                });
                ClaimResult::Run(ClaimedWork {
                    work: record.work,
                    owner: record.owner,
                    key: record.key,
                    instance: record.instance,
                    worker_id,
                    worker_token,
                })
            }
            _ => {
                self.discard_pending_record(record_id, record);
                ClaimResult::Stale
            }
        }
    }

    pub fn finish(self, work: &Work, claimed: ClaimedWork) -> FinishResult {
        if claimed.work != work.key() {
            return FinishResult::Stale;
        }
        let (color, active, canceling, requeue) = {
            let mut work_guard = work.state().lock();
            match work_guard.kind {
                WorkStateKind::Running(running)
                    if running.owner == claimed.owner
                        && running.key == claimed.key
                        && running.instance == claimed.instance
                        && running.worker_id == claimed.worker_id
                        && running.worker_token == claimed.worker_token =>
                {
                    if let Some(requeue) = running.requeue {
                        work_guard.kind = WorkStateKind::Pending(PendingWorkState {
                            owner: requeue.owner,
                            record: requeue.record,
                            instance: requeue.instance,
                        });
                    } else {
                        work_guard.kind = WorkStateKind::Idle;
                    }
                    (
                        running.color,
                        running.active,
                        running.canceling,
                        running.requeue,
                    )
                }
                _ => return FinishResult::Stale,
            }
        };

        let promote_budget = self.complete_accounting(color, active);
        FinishResult::Finished {
            requeue_op: requeue
                .map(|requeue| ExecutorOp::EnqueueInactive(self.entry_for_requeue(requeue))),
            promote_op: self.promote_op(promote_budget),
            cancel_complete: canceling,
        }
    }

    pub fn cancel_pending(self, work: &Work) -> CancelPendingResult {
        let pending = {
            let mut work_guard = work.state().lock();
            match work_guard.kind {
                WorkStateKind::Pending(pending) if pending.owner == self.owner() => {
                    work_guard.kind = WorkStateKind::Idle;
                    pending
                }
                WorkStateKind::Idle => return CancelPendingResult::NotPending,
                _ => return CancelPendingResult::Busy,
            }
        };

        let removed = {
            let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            binding.pending.remove(pending.record)
        };
        let Some(record) = removed else {
            return CancelPendingResult::Busy;
        };
        let entry = ExecutorEntry::new(
            self.binding_id(),
            record.owner,
            record.key,
            pending.record.payload(),
        );
        let promote_budget = self.complete_accounting(record.color, record.active);
        CancelPendingResult::Canceled {
            remove_op: ExecutorOp::Remove(entry),
            promote_op: self.promote_op(promote_budget),
        }
    }

    pub fn cancel_work(self, work: &Work) -> CancelWorkResult {
        let owner = self.owner();
        let pending = {
            let mut work_guard = work.state().lock();
            match work_guard.kind {
                WorkStateKind::Pending(pending) if pending.owner == owner => {
                    work_guard.kind = WorkStateKind::Idle;
                    Some(pending)
                }
                WorkStateKind::DelayedPending { target_owner } if target_owner == owner => {
                    work_guard.kind = WorkStateKind::Idle;
                    return CancelWorkResult::CanceledDelayed;
                }
                WorkStateKind::Running(ref mut running) if running.owner == owner => {
                    if let Some(requeue) = running.requeue.take() {
                        let remove_op = self.cancel_requeue(requeue);
                        debug_assert!(matches!(remove_op, ExecutorOp::Remove(_)));
                    }
                    running.canceling = true;
                    return CancelWorkResult::WaitingRunning(WorkFlushSnapshot {
                        work: work.key(),
                        owner,
                        instance: Some(running.instance),
                        complete: false,
                    });
                }
                WorkStateKind::Idle => return CancelWorkResult::NotPending,
                _ => return CancelWorkResult::Busy,
            }
        };

        let pending = pending.expect("pending cancel branch returns a pending state");
        let removed = {
            let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            binding.pending.remove(pending.record)
        };
        let Some(record) = removed else {
            return CancelWorkResult::Busy;
        };
        let entry = ExecutorEntry::new(
            self.binding_id(),
            record.owner,
            record.key,
            pending.record.payload(),
        );
        let promote_budget = self.complete_accounting(record.color, record.active);
        CancelWorkResult::CanceledPending {
            remove_op: ExecutorOp::Remove(entry),
            promote_op: self.promote_op(promote_budget),
        }
    }

    /// Attempts to cancel queued or delayed work without waiting.
    ///
    /// Running work is reported but left unmodified. Use [`Self::cancel_work`]
    /// when a synchronous caller intends to wait for a running callback.
    pub fn cancel_work_nonblocking(self, work: &Work) -> CancelWorkResult {
        let owner = self.owner();
        let pending = {
            let mut work_guard = work.state().lock();
            match work_guard.kind {
                WorkStateKind::Pending(pending) if pending.owner == owner => {
                    work_guard.kind = WorkStateKind::Idle;
                    Some(pending)
                }
                WorkStateKind::DelayedPending { target_owner } if target_owner == owner => {
                    work_guard.kind = WorkStateKind::Idle;
                    return CancelWorkResult::CanceledDelayed;
                }
                WorkStateKind::Running(ref mut running) if running.owner == owner => {
                    if let Some(requeue) = running.requeue.take() {
                        return CancelWorkResult::CanceledRunningRequeue {
                            remove_op: self.cancel_requeue(requeue),
                        };
                    }
                    return CancelWorkResult::WaitingRunning(WorkFlushSnapshot {
                        work: work.key(),
                        owner,
                        instance: Some(running.instance),
                        complete: false,
                    });
                }
                WorkStateKind::Idle => return CancelWorkResult::NotPending,
                _ => return CancelWorkResult::Busy,
            }
        };

        let pending = pending.expect("pending cancel branch returns a pending state");
        let removed = {
            let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            binding.pending.remove(pending.record)
        };
        let Some(record) = removed else {
            return CancelWorkResult::Busy;
        };
        let entry = ExecutorEntry::new(
            self.binding_id(),
            record.owner,
            record.key,
            pending.record.payload(),
        );
        let promote_budget = self.complete_accounting(record.color, record.active);
        CancelWorkResult::CanceledPending {
            remove_op: ExecutorOp::Remove(entry),
            promote_op: self.promote_op(promote_budget),
        }
    }

    pub fn commit_promoted(self, entry: ExecutorEntry, work: &Work) -> bool {
        if entry.binding != self.binding_id() || entry.owner != self.owner() {
            return false;
        }
        let record_id = PendingRecordId::from_payload(entry.payload);
        let work_guard = work.state().lock();
        let WorkStateKind::Pending(pending) = work_guard.kind else {
            return false;
        };
        if pending.owner != entry.owner || pending.record != record_id {
            return false;
        }

        let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        let Some(record) = binding.pending.get_mut(record_id) else {
            return false;
        };
        if record.work != work.key() || record.owner != entry.owner || record.key != entry.key {
            return false;
        }
        if record.active {
            return false;
        }
        record.active = true;
        binding.active += 1;
        true
    }

    pub fn start_flush(self) -> FlushSnapshot {
        let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        let color = binding.current_color;
        binding.current_color = binding.current_color.next();
        FlushSnapshot {
            color,
            complete: binding.in_flight[color.index()] == 0,
        }
    }

    pub fn flush_complete(self, snapshot: FlushSnapshot) -> bool {
        let binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        binding.in_flight[snapshot.color.index()] == 0
    }

    pub fn flush_work(self, work: &Work) -> WorkFlushSnapshot {
        let owner = self.owner();
        let work_guard = work.state().lock();
        let instance = match work_guard.kind {
            WorkStateKind::Pending(pending) if pending.owner == owner => Some(pending.instance),
            WorkStateKind::Running(running) if running.owner == owner => running
                .requeue
                .map(|requeue| requeue.instance)
                .or(Some(running.instance)),
            _ => None,
        };
        WorkFlushSnapshot {
            work: work.key(),
            owner,
            instance,
            complete: instance.is_none(),
        }
    }

    pub fn executor_op_for_pending_work(self, work: &Work) -> Option<ExecutorOp> {
        let work_guard = work.state().lock();
        let WorkStateKind::Pending(pending) = work_guard.kind else {
            return None;
        };
        if pending.owner != self.owner() {
            return None;
        }
        let binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        let record = binding.pending.get(pending.record)?;
        if record.work != work.key() || record.owner != pending.owner {
            return None;
        }
        let entry = ExecutorEntry::new(
            self.binding_id(),
            record.owner,
            record.key,
            pending.record.payload(),
        );
        Some(if record.active {
            ExecutorOp::EnqueueRunnable(entry)
        } else {
            ExecutorOp::EnqueueInactive(entry)
        })
    }

    pub fn flush_work_complete(self, snapshot: WorkFlushSnapshot, work: &Work) -> bool {
        if snapshot.complete {
            return true;
        }
        let Some(instance) = snapshot.instance else {
            return true;
        };
        if snapshot.work != work.key() {
            return true;
        }
        let work_guard = work.state().lock();
        !matches!(
            work_guard.kind,
            WorkStateKind::Pending(pending)
                if pending.owner == snapshot.owner && pending.instance == instance
        ) && !matches!(
            work_guard.kind,
            WorkStateKind::Running(running)
                if running.owner == snapshot.owner && running.instance == instance
        ) && !matches!(
            work_guard.kind,
            WorkStateKind::Running(running)
                if running.owner == snapshot.owner
                    && running.requeue.is_some_and(|requeue| requeue.instance == instance)
        )
    }

    pub fn snapshot(self) -> WorkQueueBindingSnapshot {
        let binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        WorkQueueBindingSnapshot {
            active: binding.active,
            current_color: binding.current_color,
            in_flight: binding.in_flight,
            pending: binding.pending.len(),
        }
    }

    fn discard_pending_record(self, record_id: PendingRecordId, record: PendingRecord) {
        let removed = {
            let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            binding.pending.remove(record_id)
        };
        if removed.is_some() {
            let _ = self.complete_accounting(record.color, record.active);
        }
    }

    fn complete_accounting(self, color: WorkColor, active: bool) -> usize {
        let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        if active {
            binding.active = binding.active.saturating_sub(1);
        }
        binding.in_flight[color.index()] = binding.in_flight[color.index()].saturating_sub(1);
        self.queue.max_active.saturating_sub(binding.active)
    }

    fn queue_running_requeue_locked(
        self,
        work: &Work,
        work_guard: &mut crate::work::WorkState,
    ) -> QueueWorkResult {
        let WorkStateKind::Running(ref running) = work_guard.kind else {
            return Err(QueueWorkError::AlreadyQueued);
        };
        if running.owner != self.owner() || running.requeue.is_some() {
            return Err(QueueWorkError::AlreadyQueued);
        }

        let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
        let instance = work_guard.alloc_instance();
        let color = binding.current_color;
        let owner = self.owner();
        let key = instance.as_key();
        let record = PendingRecord {
            work: work.key(),
            owner,
            key,
            instance,
            color,
            active: false,
        };
        let record_id = binding
            .pending
            .insert(record)
            .map_err(|_| QueueWorkError::PendingFull)?;
        binding.in_flight[color.index()] += 1;
        let requeue = crate::work::RequeueWorkState {
            owner,
            record: record_id,
            instance,
        };
        let WorkStateKind::Running(ref mut running) = work_guard.kind else {
            return Err(QueueWorkError::AlreadyQueued);
        };
        running.requeue = Some(requeue);
        Ok(QueueWorkOutcome::QueuedWhileRunning)
    }

    fn cancel_requeue(self, requeue: crate::work::RequeueWorkState) -> ExecutorOp {
        let removed = {
            let mut binding = self.queue.bindings[self.cpu_id.as_usize()].lock();
            binding.pending.remove(requeue.record)
        };
        let Some(record) = removed else {
            return ExecutorOp::Remove(self.entry_for_requeue(requeue));
        };
        let _ = self.complete_accounting(record.color, record.active);
        ExecutorOp::Remove(ExecutorEntry::new(
            self.binding_id(),
            record.owner,
            record.key,
            requeue.record.payload(),
        ))
    }

    fn entry_for_requeue(self, requeue: crate::work::RequeueWorkState) -> ExecutorEntry {
        let key = self.queue.bindings[self.cpu_id.as_usize()]
            .lock()
            .pending
            .get(requeue.record)
            .map(|record| record.key)
            .unwrap_or_else(|| requeue.instance.as_key());
        ExecutorEntry::new(
            self.binding_id(),
            requeue.owner,
            key,
            requeue.record.payload(),
        )
    }

    fn promote_op(self, budget: usize) -> Option<ExecutorOp> {
        (budget > 0).then_some(ExecutorOp::PromoteInactive {
            owner: self.owner(),
            budget,
        })
    }
}

pub struct WorkQueueBindingState<const PENDING_CAP: usize> {
    active: usize,
    current_color: WorkColor,
    in_flight: [usize; WorkColor::COUNT],
    pending: PendingRecordTable<PENDING_CAP>,
}

/// Read-only workqueue binding snapshot for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkQueueBindingSnapshot {
    pub active: usize,
    pub current_color: WorkColor,
    pub in_flight: [usize; WorkColor::COUNT],
    pub pending: usize,
}

impl<const PENDING_CAP: usize> WorkQueueBindingState<PENDING_CAP> {
    const fn new() -> Self {
        Self {
            active: 0,
            current_color: WorkColor::DEFAULT,
            in_flight: [0; WorkColor::COUNT],
            pending: PendingRecordTable::new(),
        }
    }
}

pub type QueueWorkResult = Result<QueueWorkOutcome, QueueWorkError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueWorkOutcome {
    Runnable(ExecutorOp),
    Inactive(ExecutorOp),
    QueuedWhileRunning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueWorkError {
    AlreadyQueued,
    Disabled,
    NotPendingDelayed,
    PendingFull,
}

#[derive(Clone, Copy)]
pub enum ClaimResult {
    Run(ClaimedWork),
    Stale,
}

#[derive(Clone, Copy)]
pub struct ClaimedWork {
    work: crate::WorkKey,
    owner: EntryOwner,
    key: crate::EntryKey,
    instance: WorkInstanceId,
    worker_id: usize,
    worker_token: usize,
}

impl ClaimedWork {
    pub const fn work_key(self) -> crate::WorkKey {
        self.work
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishResult {
    Finished {
        requeue_op: Option<ExecutorOp>,
        promote_op: Option<ExecutorOp>,
        cancel_complete: bool,
    },
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelPendingResult {
    Canceled {
        remove_op: ExecutorOp,
        promote_op: Option<ExecutorOp>,
    },
    NotPending,
    Busy,
}

#[derive(Clone, Copy)]
pub enum CancelWorkResult {
    CanceledPending {
        remove_op: ExecutorOp,
        promote_op: Option<ExecutorOp>,
    },
    CanceledRunningRequeue {
        remove_op: ExecutorOp,
    },
    CanceledDelayed,
    WaitingRunning(WorkFlushSnapshot),
    NotPending,
    Busy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushSnapshot {
    color: WorkColor,
    complete: bool,
}

impl FlushSnapshot {
    pub const fn complete(self) -> bool {
        self.complete
    }
}

#[derive(Clone, Copy)]
pub struct WorkFlushSnapshot {
    work: crate::WorkKey,
    owner: EntryOwner,
    instance: Option<WorkInstanceId>,
    complete: bool,
}

impl WorkFlushSnapshot {
    pub const fn complete(self) -> bool {
        self.complete
    }
}
