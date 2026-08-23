// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;
use kpoll::{Completion, PollEvent};
use kspin::SpinNoIrq;

use super::{
    CancelWorkResult, RunQueueEntryClaim, WorkColor, WorkInstanceId, WorkState, WorkStatus,
    WorkqueueError,
};
use crate::{
    BottomHalfWorkQueueKind, DeferredWake, PendingCancel, QueueWorkResult, SystemWorkQueueKind,
    WorkQueue, WorkQueueHandle, WorkQueuePoolBinding, WorkQueueRuntime, WorkerExecutionToken,
    WorkerId, WorkqueueSyncWaitIf, WorkqueueTaskContextIf, attach_flush_barrier,
    cancel_pending_from_binding, finish_workqueue_pool_enqueue, reject_invalid_wait_context,
    reject_self_wait, schedule_long_work, schedule_long_work_on, schedule_work, schedule_work_on,
    system_bh_highpri_wq, system_bh_highpri_wq_for_cpu, system_bh_wq, system_bh_wq_for_cpu,
};

/// Built-in queue target selected for one schedule operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleTarget {
    /// Task-context system workerqueue.
    System(SystemWorkQueueKind),
    /// Bottom-half workerqueue.
    BottomHalf(BottomHalfWorkQueueKind),
}

#[derive(Clone)]
pub(crate) enum ScheduleQueue {
    Builtin(ScheduleTarget),
    Static(&'static WorkQueue),
    Dynamic(WorkQueueHandle),
}

mod schedule_queue_ref_private {
    use crate::{WorkQueue, WorkQueueHandle};

    pub trait Sealed {}

    impl Sealed for &'static WorkQueue {}
    impl Sealed for WorkQueueHandle {}
    impl Sealed for &WorkQueueHandle {}
}

/// Custom workerqueue reference accepted by schedule-time APIs.
///
/// Implemented for fixed static queues and refcounted dynamic queue handles.
/// The trait is sealed so external code can schedule only onto queue owner
/// types whose lifecycle is understood by kwork.
pub trait ScheduleQueueRef: schedule_queue_ref_private::Sealed {
    #[doc(hidden)]
    fn __schedule_attrs(self) -> ScheduleAttrs
    where
        Self: Sized;
}

impl ScheduleQueueRef for &'static WorkQueue {
    fn __schedule_attrs(self) -> ScheduleAttrs {
        ScheduleAttrs {
            queue: ScheduleQueue::Static(self),
            cpu_id: None,
        }
    }
}

impl ScheduleQueueRef for WorkQueueHandle {
    fn __schedule_attrs(self) -> ScheduleAttrs {
        ScheduleAttrs {
            queue: ScheduleQueue::Dynamic(self),
            cpu_id: None,
        }
    }
}

impl ScheduleQueueRef for &WorkQueueHandle {
    fn __schedule_attrs(self) -> ScheduleAttrs {
        ScheduleAttrs {
            queue: ScheduleQueue::Dynamic(self.clone()),
            cpu_id: None,
        }
    }
}

/// Queue-selection attributes for one schedule operation.
///
/// A [`ScheduledWork`] owns the callback and per-instance lifecycle state. The
/// target queue is chosen here, when that instance is queued, so callers can
/// reuse one preallocated work instance with different schedule-time policy.
#[derive(Clone)]
pub struct ScheduleAttrs {
    queue: ScheduleQueue,
    cpu_id: Option<LogicalCpuId>,
}

impl ScheduleAttrs {
    /// Creates default task-context system schedule attributes.
    pub const fn new() -> Self {
        Self {
            queue: ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Default)),
            cpu_id: None,
        }
    }

    /// Selects the default task-context system workerqueue.
    pub const fn system() -> Self {
        Self::new()
    }

    /// Selects the long-running task-context system workerqueue.
    pub const fn long_system() -> Self {
        Self {
            queue: ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Long)),
            cpu_id: None,
        }
    }

    /// Selects the default bottom-half workerqueue.
    pub const fn bottom_half() -> Self {
        Self {
            queue: ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::Default,
            )),
            cpu_id: None,
        }
    }

    /// Selects the high-priority bottom-half workerqueue.
    pub const fn bottom_half_highpri() -> Self {
        Self {
            queue: ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::HighPri,
            )),
            cpu_id: None,
        }
    }

    /// Selects a custom workqueue.
    pub fn queue(queue: impl ScheduleQueueRef) -> Self {
        queue.__schedule_attrs()
    }

    /// Binds this schedule attempt to `cpu_id`.
    pub const fn on_cpu(mut self, cpu_id: LogicalCpuId) -> Self {
        self.cpu_id = Some(cpu_id);
        self
    }

    pub(crate) fn target(self) -> ScheduleQueue {
        self.queue
    }

    pub(crate) const fn cpu_id(&self) -> Option<LogicalCpuId> {
        self.cpu_id
    }
}

impl Default for ScheduleAttrs {
    fn default() -> Self {
        Self::new()
    }
}

/// Workerqueue callback.
///
/// The callback runs in sleepable task context after the shared worker-pool
/// drain path has dropped all kwork locks. Bottom-half work uses a separate
/// execution domain and callback contract.
type WorkFunc = dyn Fn(&ScheduledWork) + Send + Sync + 'static;

pub(crate) struct WorkGate {
    disable_depth: usize,
}

impl WorkGate {
    const MAX_DISABLE_DEPTH: usize = usize::MAX;

    const fn new() -> Self {
        Self { disable_depth: 0 }
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disable_depth != 0
    }

    pub(crate) fn disable(&mut self) {
        if self.disable_depth == Self::MAX_DISABLE_DEPTH {
            warn!("workerqueue work disable count overflowed");
            return;
        }
        self.disable_depth += 1;
    }

    pub(crate) fn enable(&mut self) -> bool {
        if self.disable_depth == 0 {
            warn!("workerqueue work disable count underflowed");
            return false;
        }
        self.disable_depth -= 1;
        self.disable_depth == 0
    }
}

pub(crate) struct ScheduledWorkInner<F: ?Sized = WorkFunc> {
    pub(crate) gate: SpinNoIrq<WorkGate>,
    pub(crate) state: SpinNoIrq<WorkState>,
    pub(crate) done: Completion,
    pub(crate) state_change: PollEvent,
    func: F,
}

pub(crate) struct FinishedRunningWork {
    pub(crate) work_waiters: kpoll::PollSet,
    pub(crate) binding: Option<WorkQueuePoolBinding>,
    pub(crate) instance_id: Option<WorkInstanceId>,
    pub(crate) pool_key: usize,
    pub(crate) color: WorkColor,
    pub(crate) worker_id: Option<WorkerId>,
    pub(crate) worker_token: Option<WorkerExecutionToken>,
}

impl ScheduledWorkInner {
    fn new(func: impl Fn(&ScheduledWork) + Send + Sync + 'static) -> Arc<Self> {
        let done = Completion::new();
        done.complete_all();
        let inner: Arc<ScheduledWorkInner> = Arc::new(ScheduledWorkInner {
            gate: SpinNoIrq::new(WorkGate::new()),
            state: SpinNoIrq::new(WorkState::new()),
            done,
            state_change: PollEvent::new(),
            func,
        });
        inner
    }
}

/// A refcounted handle to one scheduled workerqueue instance.
///
/// Cloning this handle does not allocate. Queue entries and running callbacks
/// hold their own handles, so the instance state remains alive until it is
/// neither queued nor running. Owners that need deterministic teardown should
/// call [`Self::cancel_sync`] or [`Self::flush`] before dropping their last
/// handle.
#[derive(Clone)]
pub struct ScheduledWork {
    inner: Arc<ScheduledWorkInner>,
}

impl ScheduledWork {
    /// Creates an idle scheduled-work instance.
    ///
    /// This may allocate for the callback and wake sources, so callers that
    /// need IRQ-safe enqueue must create the instance before entering
    /// interrupt-like context. Subsequent [`Self::schedule`] and
    /// [`Self::schedule_with`] calls do not allocate.
    pub fn new(func: impl Fn(&ScheduledWork) + Send + Sync + 'static) -> Self {
        Self {
            inner: ScheduledWorkInner::new(func),
        }
    }

    pub(crate) fn inner(&self) -> &ScheduledWorkInner {
        &self.inner
    }

    pub(crate) fn run(&self) {
        (self.inner.func)(self);
    }

    pub(crate) fn key(&self) -> usize {
        Arc::as_ptr(&self.inner).addr()
    }

    pub(crate) fn same_work(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn gate(&self) -> &SpinNoIrq<WorkGate> {
        &self.inner.gate
    }

    pub(crate) fn notify_state_change_defer(&self) -> kpoll::PollSet {
        self.inner.state_change.notify_defer_wake()
    }

    pub(crate) fn complete_done_defer_wake(&self) -> kpoll::PollSet {
        self.inner.done.complete_all_defer_wake()
    }

    pub(crate) fn reject_queue_if_disabled(&self) -> Result<(), QueueWorkResult> {
        if self.inner.gate.lock().is_disabled() {
            Err(QueueWorkResult::Disabled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn queue_new_pending_with(
        &self,
        binding: WorkQueuePoolBinding,
        color: WorkColor,
        insert: impl FnOnce(WorkInstanceId) -> Result<(), QueueWorkResult>,
    ) -> Result<WorkInstanceId, QueueWorkResult> {
        let mut state = self.inner.state.lock();
        state.can_queue_now()?;
        let instance_id = state.allocate_instance_id();
        insert(instance_id)?;
        self.inner.done.reinit();
        state.set_pending(instance_id, binding, color);
        Ok(instance_id)
    }

    pub(crate) fn queue_reserved_pending_with(
        &self,
        instance_id: WorkInstanceId,
        binding: WorkQueuePoolBinding,
        color: WorkColor,
        insert: impl FnOnce() -> Result<(), QueueWorkResult>,
    ) -> Result<(), QueueWorkResult> {
        let mut state = self.inner.state.lock();
        if state.status() != super::WorkStatus::DelayedPending
            || state.pending_instance_id() != Some(instance_id)
        {
            return Err(QueueWorkResult::AlreadyQueued);
        }
        insert()?;
        state.set_pending(instance_id, binding, color);
        Ok(())
    }

    pub(crate) fn claim_pool_entry_for_run(
        &self,
        pool_key: usize,
        binding_key: usize,
        instance_id: WorkInstanceId,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
    ) -> RunQueueEntryClaim {
        self.inner.state.lock().claim_pool_entry_for_run(
            pool_key,
            binding_key,
            instance_id,
            worker_id,
            worker_token,
        )
    }

    pub(crate) fn finish_running_state(&self) -> Result<FinishedRunningWork, WorkStatus> {
        let mut state = self.inner.state.lock();
        if state.status() != WorkStatus::Running {
            return Err(state.status());
        }

        let finished = FinishedRunningWork {
            work_waiters: self.inner.done.complete_all_defer_wake(),
            binding: state.running_binding_cloned(),
            instance_id: state.running_instance_id(),
            pool_key: state.running_pool_key(),
            color: state.running_color(),
            worker_id: state.running_worker_id(),
            worker_token: state.running_worker_token(),
        };
        state.set_idle();
        Ok(finished)
    }

    /// Queues this preallocated instance on the default system workerqueue.
    ///
    /// This enqueue path does not allocate and may be used by interrupt-like
    /// producers. The same instance cannot be queued again while it is already
    /// pending or running.
    pub fn schedule(&self) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::new())
    }

    /// Queues this preallocated instance using schedule-time attributes.
    pub fn schedule_with(&self, attrs: ScheduleAttrs) -> QueueWorkResult {
        let cpu_id = attrs.cpu_id();
        match attrs.target() {
            ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Default)) => {
                match cpu_id {
                    Some(cpu_id) => schedule_work_on(cpu_id, self),
                    None => schedule_work(self),
                }
            }
            ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Long)) => {
                match cpu_id {
                    Some(cpu_id) => schedule_long_work_on(cpu_id, self),
                    None => schedule_long_work(self),
                }
            }
            ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::Default,
            )) => match cpu_id {
                Some(cpu_id) => match system_bh_wq_for_cpu(cpu_id) {
                    Some(queue) => queue.queue_work(self),
                    None => QueueWorkResult::InvalidCpu,
                },
                None => system_bh_wq().queue_work(self),
            },
            ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::HighPri,
            )) => match cpu_id {
                Some(cpu_id) => match system_bh_highpri_wq_for_cpu(cpu_id) {
                    Some(queue) => queue.queue_work(self),
                    None => QueueWorkResult::InvalidCpu,
                },
                None => system_bh_highpri_wq().queue_work(self),
            },
            ScheduleQueue::Static(queue) => match cpu_id {
                Some(cpu_id) => match queue.select_pool_binding(Some(cpu_id)) {
                    Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(self)),
                    Err(result) => result,
                },
                None => queue.queue_work(self),
            },
            ScheduleQueue::Dynamic(queue) => match cpu_id {
                Some(cpu_id) => match queue.clone().select_pool_binding(Some(cpu_id)) {
                    Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(self)),
                    Err(result) => result,
                },
                None => queue.queue_work(self),
            },
        }
    }

    /// Queues this instance on the target CPU's default system workerqueue.
    pub fn schedule_on(&self, cpu_id: LogicalCpuId) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::system().on_cpu(cpu_id))
    }

    /// Queues this instance on the long-running system workerqueue.
    pub fn schedule_long(&self) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::long_system())
    }

    /// Queues this instance on the target CPU's long-running system workerqueue.
    pub fn schedule_long_on(&self, cpu_id: LogicalCpuId) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::long_system().on_cpu(cpu_id))
    }

    /// Queues this instance on a custom workqueue.
    pub fn schedule_on_queue(&self, queue: impl ScheduleQueueRef) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::queue(queue))
    }

    /// Cancels a queued instance without waiting for a running callback.
    ///
    /// Returns [`CancelWorkResult::CancelledPending`] when a pending queued
    /// instance was removed, [`CancelWorkResult::NotPending`] when no queued
    /// instance existed, and [`CancelWorkResult::Running`] when the callback
    /// is currently running (or reserved as a delayed timer instance). This
    /// call never blocks and is valid from interrupt-like context.
    pub fn cancel(&self) -> CancelWorkResult {
        let (result, removed, waiters) = loop {
            let pending_binding = {
                let work_state = self.inner.state.lock();
                match work_state.status() {
                    super::WorkStatus::Pending => work_state.pending_binding_cloned(),
                    super::WorkStatus::DelayedPending => {
                        break (CancelWorkResult::NotPending, None, DeferredWake::default());
                    }
                    super::WorkStatus::Running => {
                        break (CancelWorkResult::Running, None, DeferredWake::default());
                    }
                    super::WorkStatus::Idle => {
                        break (CancelWorkResult::NotPending, None, DeferredWake::default());
                    }
                }
            };

            let Some(binding) = pending_binding else {
                warn!("queued work has no pool binding during cancel");
                break (CancelWorkResult::NotPending, None, DeferredWake::default());
            };

            match cancel_pending_from_binding(binding, self, false) {
                PendingCancel::Done(result, removed, waiters) => {
                    break (result, removed, waiters);
                }
                PendingCancel::Retry => continue,
            }
        };

        drop(removed);
        waiters.wake();
        result
    }

    /// Cancels this work and waits for a running callback to finish.
    ///
    /// Returns `Ok(true)` when a queued or running instance existed and was
    /// cancelled or waited out, and `Ok(false)` when the work was already
    /// idle. Fails with [`WorkqueueError::InvalidContext`] in interrupt-like
    /// context, [`WorkqueueError::SelfWait`] when the current worker callback
    /// would wait on itself or its own execution pool, and
    /// [`WorkqueueError::WaitFailed`] when the wait provider fails.
    pub fn cancel_sync(&self) -> Result<bool, WorkqueueError> {
        reject_invalid_wait_context()?;
        reject_self_wait(self)?;

        let (must_wait, removed, waiters) = loop {
            let pending_binding = {
                let mut work_state = self.inner.state.lock();
                match work_state.status() {
                    super::WorkStatus::Idle => return Ok(false),
                    super::WorkStatus::DelayedPending => return Ok(false),
                    super::WorkStatus::Running => {
                        work_state.cancel_running();
                        break (true, None, DeferredWake::default());
                    }
                    super::WorkStatus::Pending => work_state.pending_binding_cloned(),
                }
            };

            let Some(binding) = pending_binding else {
                warn!("queued work has no pool binding during cancel_sync");
                return Ok(false);
            };

            match cancel_pending_from_binding(binding, self, true) {
                PendingCancel::Done(result, removed, waiters) => {
                    break (result == CancelWorkResult::Running, removed, waiters);
                }
                PendingCancel::Retry => continue,
            }
        };

        drop(removed);
        waiters.wake();

        if must_wait {
            let worker_context = WorkqueueTaskContextIf::current_work_context();
            if let Some(barrier) = attach_flush_barrier(self, worker_context)? {
                WorkqueueSyncWaitIf::wait_for_completion(barrier.completion())
                    .map_err(|_| super::WorkqueueError::WaitFailed)?;
            }
        }
        Ok(true)
    }

    /// Disables the template that created this instance and cancels it if
    /// queued.
    ///
    /// Returns the same result variants as [`Self::cancel`]. New enqueue
    /// attempts return [`crate::QueueWorkResult::Disabled`] until a matching
    /// [`Self::enable`].
    pub fn disable(&self) -> CancelWorkResult {
        self.inner.gate.lock().disable();
        self.cancel()
    }

    /// Disables the template that created this instance and waits for this
    /// instance if it is running.
    pub fn disable_sync(&self) -> Result<bool, WorkqueueError> {
        self.inner.gate.lock().disable();
        self.cancel_sync()
    }

    /// Re-enables the template that created this instance.
    pub fn enable(&self) -> bool {
        self.inner.gate.lock().enable()
    }

    /// Waits for the queued or running instance observed when the call starts.
    ///
    /// Returns `Ok(true)` when an instance was attached and waited out, and
    /// `Ok(false)` when the work had no queued or running instance at call
    /// time. Work queued after this call starts is not waited for. Fails with
    /// [`WorkqueueError::InvalidContext`] in interrupt-like context,
    /// [`WorkqueueError::SelfWait`] when the current worker callback would
    /// wait on itself or its own execution pool, and
    /// [`WorkqueueError::BarrierFull`] when the bounded barrier storage for
    /// the observed instance is full, and
    /// [`WorkqueueError::WaitFailed`] when the wait provider fails.
    pub fn flush(&self) -> Result<bool, WorkqueueError> {
        reject_invalid_wait_context()?;
        reject_self_wait(self)?;
        let worker_context = WorkqueueTaskContextIf::current_work_context();

        let Some(barrier) = attach_flush_barrier(self, worker_context)? else {
            return Ok(false);
        };
        WorkqueueSyncWaitIf::wait_for_completion(barrier.completion())
            .map_err(|_| super::WorkqueueError::WaitFailed)?;
        Ok(true)
    }
}
