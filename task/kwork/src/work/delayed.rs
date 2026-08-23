// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use kpoll::Completion;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;

use super::{
    CancelWorkResult, ScheduleAttrs, ScheduleQueue, ScheduleTarget, ScheduledWork, WorkInstanceId,
    WorkStatus, WorkqueueError,
};
use crate::{
    BottomHalfWorkQueueKind, DeferredWake, FlushTarget, QueueDelayedWorkResult, QueueWorkResult,
    SystemWorkQueueKind, WorkQueue, WorkQueueHandle, WorkQueueRuntime, WorkqueueContextIf,
    WorkqueueHostIf, WorkqueueSyncWaitIf, WorkqueueTaskContextIf, WorkqueueTimerHandle,
    WorkqueueTimerIf, finish_workqueue_pool_enqueue, reject_delayed_target_wait_deadlock,
    reject_invalid_wait_context, reject_self_wait, reject_worker_wait_deadlock,
    system_bh_highpri_wq_for_cpu, system_bh_wq_for_cpu, system_long_wq_for_cpu,
    system_percpu_wq_for_cpu,
};

#[derive(Clone)]
pub(crate) enum DelayedWorkTarget {
    Static(&'static WorkQueue),
    StaticOn {
        queue: &'static WorkQueue,
        cpu_id: LogicalCpuId,
    },
    Dynamic(WorkQueueHandle),
    DynamicOn {
        queue: WorkQueueHandle,
        cpu_id: LogicalCpuId,
    },
}

impl DelayedWorkTarget {
    pub(crate) fn queue(&self, work: &ScheduledWork) -> QueueWorkResult {
        match self {
            Self::Static(queue) => queue.queue_work(work),
            Self::StaticOn { queue, cpu_id } => match queue.select_pool_binding(Some(*cpu_id)) {
                Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
                Err(result) => result,
            },
            Self::Dynamic(queue) => queue.queue_work(work),
            Self::DynamicOn { queue, cpu_id } => {
                match queue.clone().select_pool_binding(Some(*cpu_id)) {
                    Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
                    Err(result) => result,
                }
            }
        }
    }

    pub(crate) fn queue_reserved_work(
        &self,
        work: &ScheduledWork,
        instance_id: WorkInstanceId,
    ) -> QueueWorkResult {
        match self {
            Self::Static(queue) => match queue.select_pool_binding(None) {
                Ok(binding) => finish_workqueue_pool_enqueue(
                    binding.queue_reserved_delayed_work(work, instance_id),
                ),
                Err(result) => result,
            },
            Self::StaticOn { queue, cpu_id } => match queue.select_pool_binding(Some(*cpu_id)) {
                Ok(binding) => finish_workqueue_pool_enqueue(
                    binding.queue_reserved_delayed_work(work, instance_id),
                ),
                Err(result) => result,
            },
            Self::Dynamic(queue) => {
                finish_workqueue_pool_enqueue(match queue.clone().select_pool_binding(None) {
                    Ok(binding) => binding.queue_reserved_delayed_work(work, instance_id),
                    Err(result) => return result,
                })
            }
            Self::DynamicOn { queue, cpu_id } => finish_workqueue_pool_enqueue(
                match queue.clone().select_pool_binding(Some(*cpu_id)) {
                    Ok(binding) => binding.queue_reserved_delayed_work(work, instance_id),
                    Err(result) => return result,
                },
            ),
        }
    }

    pub(crate) fn validate_for_timer(&self) -> QueueDelayedWorkResult {
        match self {
            Self::Static(queue) => queue
                .select_pool_binding(None)
                .map_or_else(Into::into, |_| QueueDelayedWorkResult::Queued),
            Self::StaticOn { queue, cpu_id } => queue
                .select_pool_binding(Some(*cpu_id))
                .map_or_else(Into::into, |_| QueueDelayedWorkResult::Queued),
            Self::Dynamic(queue) => {
                if queue.queue().is_destroying() {
                    QueueDelayedWorkResult::Disabled
                } else {
                    QueueDelayedWorkResult::Queued
                }
            }
            Self::DynamicOn { queue, .. } => {
                if queue.queue().is_destroying() {
                    QueueDelayedWorkResult::Disabled
                } else {
                    QueueDelayedWorkResult::Queued
                }
            }
        }
    }

    pub(crate) fn pool_key(&self) -> Option<usize> {
        match self {
            Self::Static(queue) => queue
                .select_pool_binding(None)
                .ok()
                .map(|binding| binding.pool_key()),
            Self::StaticOn { queue, cpu_id } => queue
                .select_pool_binding(Some(*cpu_id))
                .ok()
                .map(|binding| binding.pool_key()),
            Self::Dynamic(queue) => queue
                .clone()
                .select_pool_binding(None)
                .ok()
                .map(|binding| binding.pool_key()),
            Self::DynamicOn { queue, cpu_id } => queue
                .clone()
                .select_pool_binding(Some(*cpu_id))
                .ok()
                .map(|binding| binding.pool_key()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DelayedWorkStatus {
    Idle,
    Pending(WorkInstanceId),
    Firing(WorkInstanceId),
    Ready(WorkInstanceId),
}

pub(crate) struct DelayedWorkState {
    pub(crate) status: DelayedWorkStatus,
    pub(crate) target: Option<DelayedWorkTarget>,
    timer_handle: Option<Arc<dyn WorkqueueTimerHandle>>,
    timer_generation: usize,
}

type ModifiedDelayedTimer = (WorkInstanceId, usize, Option<Arc<dyn WorkqueueTimerHandle>>);

impl DelayedWorkState {
    const fn new() -> Self {
        Self {
            status: DelayedWorkStatus::Idle,
            target: None,
            timer_handle: None,
            timer_generation: 0,
        }
    }

    pub(crate) fn is_queued(&self) -> bool {
        self.status != DelayedWorkStatus::Idle
    }

    pub(crate) fn arm(
        &mut self,
        target: DelayedWorkTarget,
        instance_id: WorkInstanceId,
    ) -> Result<usize, QueueDelayedWorkResult> {
        if self.is_queued() {
            return Err(QueueDelayedWorkResult::AlreadyQueued);
        }
        self.status = DelayedWorkStatus::Pending(instance_id);
        self.target = Some(target);
        self.timer_handle = None;
        Ok(self.advance_timer_generation())
    }

    pub(crate) fn modify(&mut self, target: DelayedWorkTarget) -> Option<ModifiedDelayedTimer> {
        let instance_id = match self.status {
            DelayedWorkStatus::Pending(instance_id) | DelayedWorkStatus::Ready(instance_id) => {
                instance_id
            }
            DelayedWorkStatus::Idle | DelayedWorkStatus::Firing(_) => return None,
        };
        self.status = DelayedWorkStatus::Pending(instance_id);
        self.target = Some(target);
        let old_timer = self.timer_handle.take();
        let timer_generation = self.advance_timer_generation();
        Some((instance_id, timer_generation, old_timer))
    }

    pub(crate) fn install_timer_handle(
        &mut self,
        instance_id: WorkInstanceId,
        timer_generation: usize,
        handle: Arc<dyn WorkqueueTimerHandle>,
    ) -> bool {
        if matches!(self.status, DelayedWorkStatus::Pending(pending_id) if pending_id == instance_id)
            && self.timer_generation == timer_generation
        {
            self.timer_handle = Some(handle);
            return true;
        }
        false
    }

    fn advance_timer_generation(&mut self) -> usize {
        self.timer_generation = self.timer_generation.wrapping_add(1).max(1);
        self.timer_generation
    }

    pub(crate) fn timer_generation(&self) -> usize {
        self.timer_generation
    }

    pub(crate) fn begin_fire(
        &mut self,
        instance_id: WorkInstanceId,
    ) -> Option<(DelayedWorkTarget, Option<Arc<dyn WorkqueueTimerHandle>>)> {
        if matches!(
            self.status,
            DelayedWorkStatus::Pending(pending_id) | DelayedWorkStatus::Ready(pending_id)
                if pending_id == instance_id
        ) {
            self.status = DelayedWorkStatus::Firing(instance_id);
            let timer_handle = self.timer_handle.take();
            return self.target.clone().map(|target| (target, timer_handle));
        }
        None
    }

    pub(crate) fn begin_timer_fire(
        &mut self,
        instance_id: WorkInstanceId,
        timer_generation: usize,
    ) -> Option<DelayedWorkTarget> {
        if matches!(self.status, DelayedWorkStatus::Pending(pending_id) if pending_id == instance_id)
            && self.timer_generation == timer_generation
        {
            self.status = DelayedWorkStatus::Firing(instance_id);
            self.timer_handle = None;
            return self.target.clone();
        }
        None
    }

    pub(crate) fn finish_fire(
        &mut self,
        instance_id: WorkInstanceId,
        target: DelayedWorkTarget,
        outcome: DelayedFireOutcome,
    ) -> bool {
        if self.status != DelayedWorkStatus::Firing(instance_id) {
            return false;
        }
        match outcome {
            DelayedFireOutcome::Queued | DelayedFireOutcome::Clear => {
                self.status = DelayedWorkStatus::Idle;
                self.target = None;
                self.timer_handle = None;
            }
            DelayedFireOutcome::KeepReady => {
                self.status = DelayedWorkStatus::Ready(instance_id);
                self.target = Some(target);
                self.timer_handle = None;
            }
        }
        true
    }

    pub(crate) fn cancel_queueable(
        &mut self,
    ) -> Option<(WorkInstanceId, Option<Arc<dyn WorkqueueTimerHandle>>)> {
        if !matches!(
            self.status,
            DelayedWorkStatus::Pending(_) | DelayedWorkStatus::Ready(_)
        ) {
            return None;
        }
        let instance_id = match self.status {
            DelayedWorkStatus::Pending(instance_id) | DelayedWorkStatus::Ready(instance_id) => {
                instance_id
            }
            DelayedWorkStatus::Idle | DelayedWorkStatus::Firing(_) => unreachable!(),
        };
        self.status = DelayedWorkStatus::Idle;
        let timer_handle = self.timer_handle.take();
        self.target
            .take()
            .expect("queued delayed work should have a target");
        Some((instance_id, timer_handle))
    }

    pub(crate) fn queueable_target(&self) -> Option<(WorkInstanceId, DelayedWorkTarget)> {
        match self.status {
            DelayedWorkStatus::Pending(instance_id) | DelayedWorkStatus::Ready(instance_id) => {
                self.target.clone().map(|target| (instance_id, target))
            }
            DelayedWorkStatus::Firing(instance_id) => {
                self.target.clone().map(|target| (instance_id, target))
            }
            DelayedWorkStatus::Idle => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum DelayedFireOutcome {
    Queued,
    KeepReady,
    Clear,
}

pub(crate) struct DelayedScheduledWorkInner {
    pub(crate) work: ScheduledWork,
    pub(crate) state: SpinNoIrq<DelayedWorkState>,
    pub(crate) done: Completion,
}

impl DelayedScheduledWorkInner {
    fn new(work: ScheduledWork) -> Self {
        let done = Completion::new();
        done.complete_all();
        Self {
            work,
            state: SpinNoIrq::new(DelayedWorkState::new()),
            done,
        }
    }
}

/// A refcounted delayed scheduled-work handle.
///
/// Delayed work combines a timer with a normal [`ScheduledWork`]. When the timer
/// expires, `kwork` queues the inner work on the selected workerqueue and the
/// callback runs in that queue's drain context.
#[derive(Clone)]
pub struct DelayedScheduledWork {
    pub(crate) inner: Arc<DelayedScheduledWorkInner>,
}

impl DelayedScheduledWork {
    /// Creates an idle delayed scheduled-work instance.
    ///
    /// This may allocate for the callback, wake sources, and timer state
    /// holders. Schedule or modify the returned handle with
    /// [`Self::schedule_after`] or [`Self::schedule_after_with`].
    pub fn new(func: impl Fn(&ScheduledWork) + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(DelayedScheduledWorkInner::new(ScheduledWork::new(func))),
        }
    }

    pub(crate) fn scheduled(&self) -> &ScheduledWork {
        &self.inner.work
    }

    fn target_from_attrs(
        attrs: ScheduleAttrs,
    ) -> Result<DelayedWorkTarget, QueueDelayedWorkResult> {
        let cpu_id = attrs
            .cpu_id()
            .unwrap_or_else(WorkqueueHostIf::current_cpu_id);
        match attrs.target() {
            ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Default)) => {
                let queue =
                    system_percpu_wq_for_cpu(cpu_id).ok_or(QueueDelayedWorkResult::InvalidCpu)?;
                Ok(DelayedWorkTarget::StaticOn { queue, cpu_id })
            }
            ScheduleQueue::Builtin(ScheduleTarget::System(SystemWorkQueueKind::Long)) => {
                let queue =
                    system_long_wq_for_cpu(cpu_id).ok_or(QueueDelayedWorkResult::InvalidCpu)?;
                Ok(DelayedWorkTarget::StaticOn { queue, cpu_id })
            }
            ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::Default,
            )) => {
                let queue =
                    system_bh_wq_for_cpu(cpu_id).ok_or(QueueDelayedWorkResult::InvalidCpu)?;
                Ok(DelayedWorkTarget::StaticOn { queue, cpu_id })
            }
            ScheduleQueue::Builtin(ScheduleTarget::BottomHalf(
                BottomHalfWorkQueueKind::HighPri,
            )) => {
                let queue = system_bh_highpri_wq_for_cpu(cpu_id)
                    .ok_or(QueueDelayedWorkResult::InvalidCpu)?;
                Ok(DelayedWorkTarget::StaticOn { queue, cpu_id })
            }
            ScheduleQueue::Static(queue) => Ok(DelayedWorkTarget::StaticOn { queue, cpu_id }),
            ScheduleQueue::Dynamic(queue) => Ok(DelayedWorkTarget::DynamicOn { queue, cpu_id }),
        }
    }

    /// Queues this delayed work on the default system workerqueue.
    ///
    /// A zero delay queues the inner [`ScheduledWork`] immediately. A non-zero
    /// delay arms a timer and therefore must be called from sleepable task
    /// context.
    ///
    /// Returns [`QueueDelayedWorkResult::Queued`] on success,
    /// [`QueueDelayedWorkResult::AlreadyQueued`] when a delayed or queued
    /// instance already exists, [`QueueDelayedWorkResult::InvalidContext`]
    /// for a non-zero delay from interrupt-like context,
    /// [`QueueDelayedWorkResult::WorkerUnavailable`] while the target system
    /// pool is not installed, [`QueueDelayedWorkResult::Disabled`] for
    /// disabled work, and [`QueueDelayedWorkResult::TimerUnavailable`] when
    /// the deadline cannot be represented.
    pub fn schedule_after(&self, delay: TimeSpan) -> QueueDelayedWorkResult {
        self.schedule_after_with(delay, ScheduleAttrs::new())
    }

    /// Queues this delayed work using schedule-time queue attributes.
    pub fn schedule_after_with(
        &self,
        delay: TimeSpan,
        attrs: ScheduleAttrs,
    ) -> QueueDelayedWorkResult {
        match Self::target_from_attrs(attrs) {
            Ok(target) => queue_delayed_work_for_target(target, self, delay),
            Err(result) => result,
        }
    }

    /// Modifies the delay using the default system workerqueue.
    ///
    /// Timer-pending delayed work keeps the same queued instance. If the timer
    /// has already queued the inner work but a worker has not started it yet,
    /// this removes the pool entry and arms the requested delay again, matching
    /// Linux `mod_delayed_work_on()` after `try_to_grab_pending()` steals a
    /// worklist entry. A currently running callback still returns
    /// [`QueueDelayedWorkResult::AlreadyQueued`]. Other failure variants match
    /// [`Self::schedule_after`].
    pub fn mod_schedule_after(&self, delay: TimeSpan) -> QueueDelayedWorkResult {
        self.mod_schedule_after_with(delay, ScheduleAttrs::new())
    }

    /// Modifies the delay using schedule-time queue attributes.
    pub fn mod_schedule_after_with(
        &self,
        delay: TimeSpan,
        attrs: ScheduleAttrs,
    ) -> QueueDelayedWorkResult {
        match Self::target_from_attrs(attrs) {
            Ok(target) => mod_delayed_work_for_target(target, self, delay),
            Err(result) => result,
        }
    }

    /// Cancels the pending timer or queued delayed work without waiting.
    ///
    /// Returns [`CancelWorkResult::CancelledPending`] when a timer-pending or
    /// queued instance was removed, [`CancelWorkResult::NotPending`] when no
    /// instance existed, and [`CancelWorkResult::Running`] when the inner
    /// callback is running.
    pub fn cancel(&self) -> CancelWorkResult {
        let cancelled = loop {
            let mut state = self.inner.state.lock();
            match state.status {
                DelayedWorkStatus::Pending(_) | DelayedWorkStatus::Ready(_) => {
                    break state.cancel_queueable();
                }
                DelayedWorkStatus::Firing(_) => {
                    drop(state);
                    core::hint::spin_loop();
                }
                DelayedWorkStatus::Idle => break None,
            }
        };
        let cancelled_timer = cancelled.is_some();
        if let Some((instance_id, timer_handle)) = cancelled {
            if let Some(timer_handle) = timer_handle {
                timer_handle.cancel();
            }
            clear_delayed_reservation(self, instance_id).wake();
            let _ = self.inner.done.complete_all();
        }
        match self.scheduled().cancel() {
            CancelWorkResult::NotPending if cancelled_timer => CancelWorkResult::CancelledPending,
            result => result,
        }
    }

    /// Cancels this delayed work and waits for a running callback to finish.
    ///
    /// Returns `Ok(true)` when a delayed, queued, or running instance existed
    /// and was cancelled or waited out, and `Ok(false)` when the delayed work
    /// was already idle. Error variants match
    /// [`super::ScheduledWork::cancel_sync`].
    pub fn cancel_sync(&self) -> Result<bool, WorkqueueError> {
        reject_invalid_wait_context()?;
        let worker_context = WorkqueueTaskContextIf::current_work_context();
        let cancelled = loop {
            let action = {
                let mut state = self.inner.state.lock();
                match state.status {
                    DelayedWorkStatus::Pending(_) | DelayedWorkStatus::Ready(_) => {
                        break state.cancel_queueable();
                    }
                    DelayedWorkStatus::Firing(_) => {
                        if let Some((_, target)) = state.queueable_target() {
                            reject_delayed_target_wait_deadlock(&target, worker_context)?;
                        }
                        true
                    }
                    DelayedWorkStatus::Idle => break None,
                }
            };
            if action {
                WorkqueueSyncWaitIf::wait_for_completion(&self.inner.done)
                    .map_err(|_| WorkqueueError::WaitFailed)?;
            }
        };
        let cancelled_timer = cancelled.is_some();
        if let Some((instance_id, timer_handle)) = cancelled {
            if let Some(timer_handle) = timer_handle {
                timer_handle.cancel();
            }
            clear_delayed_reservation(self, instance_id).wake();
            let _ = self.inner.done.complete_all();
        }
        let cancelled_or_waited = self.scheduled().cancel_sync()?;
        Ok(cancelled_timer || cancelled_or_waited)
    }

    /// Disables this delayed work and cancels a pending timer or queued work.
    pub fn disable(&self) -> CancelWorkResult {
        self.scheduled().gate().lock().disable();
        self.cancel()
    }

    /// Disables this delayed work and waits for a running callback to finish.
    pub fn disable_sync(&self) -> Result<bool, WorkqueueError> {
        self.scheduled().gate().lock().disable();
        self.cancel_sync()
    }

    /// Re-enables this delayed work by decrementing its disable depth.
    pub fn enable(&self) -> bool {
        self.scheduled().gate().lock().enable()
    }

    /// Queues this delayed work immediately if needed and waits for it to finish.
    ///
    /// A timer-pending instance is converted into a runnable queue entry
    /// before waiting. Returns `Ok(true)` when an instance was flushed and
    /// `Ok(false)` when the delayed work was idle at call time. Fails with
    /// [`WorkqueueError::InvalidContext`], [`WorkqueueError::SelfWait`],
    /// [`WorkqueueError::QueueFailed`] when the immediate enqueue is
    /// rejected, [`WorkqueueError::BarrierFull`] when the bounded barrier
    /// storage for the queued/running instance is full, or
    /// [`WorkqueueError::WaitFailed`].
    pub fn flush(&self) -> Result<bool, WorkqueueError> {
        reject_invalid_wait_context()?;
        reject_self_wait(self.scheduled())?;
        let worker_context = WorkqueueTaskContextIf::current_work_context();
        {
            let work_state = self.scheduled().inner().state.lock();
            if let Some(target) = FlushTarget::from_work_state(&work_state) {
                reject_worker_wait_deadlock(&work_state, target, worker_context)?;
            }
        }

        loop {
            enum DelayedFlushAction {
                None,
                WaitFiring,
                Queue(
                    WorkInstanceId,
                    DelayedWorkTarget,
                    Option<Arc<dyn WorkqueueTimerHandle>>,
                ),
            }

            let action = {
                let mut state = self.inner.state.lock();
                match state.status {
                    DelayedWorkStatus::Pending(instance_id)
                    | DelayedWorkStatus::Ready(instance_id) => {
                        let target = state
                            .target
                            .clone()
                            .expect("queued delayed work should have a target");
                        reject_delayed_target_wait_deadlock(&target, worker_context)?;
                        let target = state
                            .begin_fire(instance_id)
                            .expect("queued delayed work should transition to firing");
                        let (target, timer_handle) = target;
                        DelayedFlushAction::Queue(instance_id, target, timer_handle)
                    }
                    DelayedWorkStatus::Firing(_) => {
                        if let Some((_, target)) = state.queueable_target() {
                            reject_delayed_target_wait_deadlock(&target, worker_context)?;
                        }
                        DelayedFlushAction::WaitFiring
                    }
                    DelayedWorkStatus::Idle => DelayedFlushAction::None,
                }
            };

            match action {
                DelayedFlushAction::None => break,
                DelayedFlushAction::WaitFiring => {
                    WorkqueueSyncWaitIf::wait_for_completion(&self.inner.done)
                        .map_err(|_| WorkqueueError::WaitFailed)?;
                }
                DelayedFlushAction::Queue(instance_id, target, timer_handle) => {
                    if let Some(timer_handle) = timer_handle {
                        timer_handle.cancel();
                    }
                    let result = target.queue_reserved_work(self.scheduled(), instance_id);
                    let outcome = match result {
                        QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => {
                            DelayedFireOutcome::Queued
                        }
                        QueueWorkResult::Disabled => DelayedFireOutcome::Clear,
                        _ => DelayedFireOutcome::KeepReady,
                    };
                    finish_delayed_fire(self, instance_id, target, outcome);
                    match result {
                        QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => break,
                        _ => {
                            warn!("flush_delayed_work could not queue delayed work: {result:?}");
                            return Err(WorkqueueError::QueueFailed);
                        }
                    }
                }
            }
        }
        self.scheduled().flush()
    }

    #[cfg(unittest)]
    pub(crate) fn fire_timer(&self, instance_id: WorkInstanceId) {
        let timer_generation = self.inner.state.lock().timer_generation();
        let _ = self.fire_timer_with_generation_and_failure_policy(
            instance_id,
            timer_generation,
            DelayedFireOutcome::KeepReady,
        );
    }

    pub(crate) fn fire_timer_with_generation(
        &self,
        instance_id: WorkInstanceId,
        timer_generation: usize,
    ) {
        let _ = self.fire_timer_with_generation_and_failure_policy(
            instance_id,
            timer_generation,
            DelayedFireOutcome::KeepReady,
        );
    }

    pub(crate) fn fire_timer_with_failure_policy(
        &self,
        instance_id: WorkInstanceId,
        failure: DelayedFireOutcome,
    ) -> Option<QueueWorkResult> {
        let timer_generation = self.inner.state.lock().timer_generation();
        self.fire_timer_with_generation_and_failure_policy(instance_id, timer_generation, failure)
    }

    pub(crate) fn fire_timer_with_generation_and_failure_policy(
        &self,
        instance_id: WorkInstanceId,
        timer_generation: usize,
        failure: DelayedFireOutcome,
    ) -> Option<QueueWorkResult> {
        let target = self
            .inner
            .state
            .lock()
            .begin_timer_fire(instance_id, timer_generation)?;

        let result = target.queue_reserved_work(&self.inner.work, instance_id);
        let outcome = match result {
            QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => DelayedFireOutcome::Queued,
            QueueWorkResult::Disabled => DelayedFireOutcome::Clear,
            _ => failure,
        };
        finish_delayed_fire(self, instance_id, target, outcome);
        if !matches!(
            result,
            QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued
        ) {
            warn!("delayed work timer could not queue work: {result:?}");
        }
        Some(result)
    }
}

pub(crate) struct DelayedTimerWake {
    pub(crate) work: DelayedScheduledWork,
    pub(crate) instance_id: WorkInstanceId,
    pub(crate) timer_generation: usize,
}

impl Wake for DelayedTimerWake {
    fn wake(self: Arc<Self>) {
        self.work
            .fire_timer_with_generation(self.instance_id, self.timer_generation);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.work
            .fire_timer_with_generation(self.instance_id, self.timer_generation);
    }
}

pub(crate) fn queue_delayed_work_for_target(
    target: DelayedWorkTarget,
    work: &DelayedScheduledWork,
    delay: TimeSpan,
) -> QueueDelayedWorkResult {
    if delay.is_zero() {
        return target.queue(work.scheduled()).into();
    }
    if WorkqueueContextIf::is_invalid_wait_context() {
        return QueueDelayedWorkResult::InvalidContext;
    }
    if let result @ (QueueDelayedWorkResult::InvalidCpu
    | QueueDelayedWorkResult::WorkerUnavailable
    | QueueDelayedWorkResult::Disabled) = target.validate_for_timer()
    {
        return result;
    }
    let Some(deadline) = WorkqueueTimerIf::monotonic_time().checked_add(delay) else {
        return QueueDelayedWorkResult::TimerUnavailable;
    };
    let (instance_id, timer_generation) = match reserve_delayed_work(&target, work) {
        Ok(reservation) => reservation,
        Err(result) => return result,
    };
    work.inner.done.reinit();
    let timer_wake = Arc::new(DelayedTimerWake {
        work: work.clone(),
        instance_id,
        timer_generation,
    });
    let Some(timer_handle) = WorkqueueTimerIf::register_timer(deadline, Waker::from(timer_wake))
    else {
        return work
            .fire_timer_with_failure_policy(instance_id, DelayedFireOutcome::KeepReady)
            .map_or(QueueDelayedWorkResult::AlreadyQueued, Into::into);
    };
    if !work.inner.state.lock().install_timer_handle(
        instance_id,
        timer_generation,
        timer_handle.clone(),
    ) {
        timer_handle.cancel();
    }
    QueueDelayedWorkResult::Queued
}

fn reserve_delayed_work(
    target: &DelayedWorkTarget,
    work: &DelayedScheduledWork,
) -> Result<(WorkInstanceId, usize), QueueDelayedWorkResult> {
    let pool_key = target
        .pool_key()
        .expect("validated delayed work target should have a pool key");
    let work_gate = work.scheduled().gate().lock();
    if work_gate.is_disabled() {
        return Err(QueueDelayedWorkResult::Disabled);
    }
    let mut delayed_state = work.inner.state.lock();
    if delayed_state.is_queued() {
        return Err(QueueDelayedWorkResult::AlreadyQueued);
    }
    let mut work_state = work.scheduled().inner().state.lock();
    let instance_id = match work_state.status() {
        WorkStatus::Idle => {
            let instance_id = work_state.allocate_instance_id();
            work.scheduled().inner().done.reinit();
            work_state.set_delayed_pending(instance_id, pool_key);
            instance_id
        }
        WorkStatus::Running => {
            if work_state.running_is_canceling() {
                return Err(QueueDelayedWorkResult::Disabled);
            }
            return Err(QueueDelayedWorkResult::AlreadyQueued);
        }
        WorkStatus::DelayedPending | WorkStatus::Pending => {
            return Err(QueueDelayedWorkResult::AlreadyQueued);
        }
    };
    let timer_generation = delayed_state.arm(target.clone(), instance_id)?;
    Ok((instance_id, timer_generation))
}

pub(crate) fn mod_delayed_work_for_target(
    target: DelayedWorkTarget,
    work: &DelayedScheduledWork,
    delay: TimeSpan,
) -> QueueDelayedWorkResult {
    if delay.is_zero() {
        return mod_delayed_work_zero_delay(target, work);
    }
    if WorkqueueContextIf::is_invalid_wait_context() {
        return QueueDelayedWorkResult::InvalidContext;
    }
    if let result @ (QueueDelayedWorkResult::InvalidCpu
    | QueueDelayedWorkResult::WorkerUnavailable
    | QueueDelayedWorkResult::Disabled) = target.validate_for_timer()
    {
        return result;
    }
    let Some(deadline) = WorkqueueTimerIf::monotonic_time().checked_add(delay) else {
        return QueueDelayedWorkResult::TimerUnavailable;
    };

    let modified = {
        let work_gate = work.scheduled().gate().lock();
        if work_gate.is_disabled() {
            return QueueDelayedWorkResult::Disabled;
        }
        loop {
            let mut delayed_state = work.inner.state.lock();
            match delayed_state.status {
                DelayedWorkStatus::Pending(_) | DelayedWorkStatus::Ready(_) => {
                    break delayed_state.modify(target.clone());
                }
                DelayedWorkStatus::Firing(_) => {
                    drop(delayed_state);
                    core::hint::spin_loop();
                }
                DelayedWorkStatus::Idle => break None,
            }
        }
    };

    let (instance_id, timer_generation, old_timer_handle) = match modified {
        Some(modified) => modified,
        None => return mod_idle_or_pool_pending_delayed_work(target, work, delay),
    };
    if let Some(timer_handle) = old_timer_handle {
        timer_handle.cancel();
    }

    let timer_wake = Arc::new(DelayedTimerWake {
        work: work.clone(),
        instance_id,
        timer_generation,
    });
    let Some(timer_handle) = WorkqueueTimerIf::register_timer(deadline, Waker::from(timer_wake))
    else {
        return work
            .fire_timer_with_generation_and_failure_policy(
                instance_id,
                timer_generation,
                DelayedFireOutcome::KeepReady,
            )
            .map_or(QueueDelayedWorkResult::AlreadyQueued, Into::into);
    };
    if !work.inner.state.lock().install_timer_handle(
        instance_id,
        timer_generation,
        timer_handle.clone(),
    ) {
        timer_handle.cancel();
    }
    QueueDelayedWorkResult::Queued
}

fn mod_delayed_work_zero_delay(
    target: DelayedWorkTarget,
    work: &DelayedScheduledWork,
) -> QueueDelayedWorkResult {
    let action = {
        let work_gate = work.scheduled().gate().lock();
        if work_gate.is_disabled() {
            return QueueDelayedWorkResult::Disabled;
        }
        loop {
            let mut delayed_state = work.inner.state.lock();
            match delayed_state.status {
                DelayedWorkStatus::Pending(instance_id) | DelayedWorkStatus::Ready(instance_id) => {
                    let (target, timer_handle) = delayed_state
                        .begin_fire(instance_id)
                        .expect("pending delayed work should enter firing for zero-delay modify");
                    break Some((instance_id, target, timer_handle));
                }
                DelayedWorkStatus::Firing(_) => {
                    drop(delayed_state);
                    core::hint::spin_loop();
                }
                DelayedWorkStatus::Idle => break None,
            }
        }
    };

    let Some((instance_id, target, timer_handle)) = action else {
        return mod_idle_or_pool_pending_delayed_work(target, work, TimeSpan::ZERO);
    };

    if let Some(timer_handle) = timer_handle {
        timer_handle.cancel();
    }
    let result = target.queue_reserved_work(work.scheduled(), instance_id);
    let outcome = match result {
        QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => DelayedFireOutcome::Queued,
        QueueWorkResult::Disabled => DelayedFireOutcome::Clear,
        _ => DelayedFireOutcome::KeepReady,
    };
    finish_delayed_fire(work, instance_id, target, outcome);
    result.into()
}

fn finish_delayed_fire(
    work: &DelayedScheduledWork,
    instance_id: WorkInstanceId,
    target: DelayedWorkTarget,
    outcome: DelayedFireOutcome,
) {
    let should_complete = work
        .inner
        .state
        .lock()
        .finish_fire(instance_id, target, outcome);
    if should_complete && matches!(outcome, DelayedFireOutcome::Clear) {
        clear_delayed_reservation(work, instance_id).wake();
    }
    if should_complete {
        let _ = work.inner.done.complete_all();
    }
}

fn mod_idle_or_pool_pending_delayed_work(
    target: DelayedWorkTarget,
    work: &DelayedScheduledWork,
    delay: TimeSpan,
) -> QueueDelayedWorkResult {
    let status = { work.scheduled().inner().state.lock().status() };
    match status {
        WorkStatus::Pending => match work.scheduled().cancel() {
            CancelWorkResult::CancelledPending | CancelWorkResult::NotPending => {
                queue_delayed_work_for_target(target, work, delay)
            }
            CancelWorkResult::Running => QueueDelayedWorkResult::AlreadyQueued,
        },
        WorkStatus::Running => QueueDelayedWorkResult::AlreadyQueued,
        WorkStatus::Idle => queue_delayed_work_for_target(target, work, delay),
        WorkStatus::DelayedPending => QueueDelayedWorkResult::AlreadyQueued,
    }
}

pub(crate) fn clear_delayed_reservation(
    work: &DelayedScheduledWork,
    instance_id: WorkInstanceId,
) -> DeferredWake {
    let mut work_state = work.scheduled().inner().state.lock();
    match work_state.status() {
        WorkStatus::DelayedPending if work_state.pending_instance_id() == Some(instance_id) => {
            work_state.set_idle();
            DeferredWake::from_work(work.scheduled().inner().done.complete_all_defer_wake())
        }
        _ => DeferredWake::default(),
    }
}
