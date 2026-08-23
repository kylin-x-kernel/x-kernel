// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[derive(Default)]
pub(crate) struct DeferredWake {
    work: Option<kpoll::PollSet>,
    queue: QueueWake,
    barriers: alloc::vec::Vec<kpoll::PollSet>,
}

impl DeferredWake {
    pub(super) fn from_work(work: kpoll::PollSet) -> Self {
        Self {
            work: Some(work),
            queue: QueueWake::default(),
            barriers: alloc::vec::Vec::new(),
        }
    }

    pub(super) fn with_queue(mut self, queue: QueueWake) -> Self {
        self.queue = queue;
        self
    }

    pub(super) fn with_work(mut self, work: kpoll::PollSet) -> Self {
        self.work = Some(work);
        self
    }

    pub(super) fn with_barriers(mut self, barriers: alloc::vec::Vec<kpoll::PollSet>) -> Self {
        self.barriers.extend(barriers);
        self
    }

    pub(crate) fn wake(self) {
        if let Some(work) = self.work {
            let _ = work.wake();
        }
        self.queue.wake();
        for barrier in self.barriers {
            let _ = barrier.wake();
        }
    }
}

pub(crate) enum PendingCancel {
    Done(CancelWorkResult, Option<ScheduledWork>, DeferredWake),
    Retry,
}

#[derive(Clone, Copy)]
pub(crate) struct FlushTarget {
    instance_id: WorkInstanceId,
    pool_key: usize,
}

impl FlushTarget {
    pub(crate) fn from_work_state(work_state: &WorkState) -> Option<Self> {
        match work_state.status() {
            WorkStatus::Idle => None,
            WorkStatus::DelayedPending | WorkStatus::Pending => Some(Self {
                instance_id: work_state
                    .pending_instance_id()
                    .expect("pending status should carry a work instance"),
                pool_key: work_state.pending_pool_key(),
            }),
            WorkStatus::Running => Some(Self {
                instance_id: work_state
                    .running_instance_id()
                    .expect("running status should carry a work instance"),
                pool_key: work_state.running_pool_key(),
            }),
        }
    }

    fn is_gone(self, work_state: &WorkState) -> bool {
        let pending_matches = matches!(
            work_state.status(),
            WorkStatus::DelayedPending | WorkStatus::Pending
        ) && work_state.pending_instance_id() == Some(self.instance_id);
        let running_matches = matches!(work_state.status(), WorkStatus::Running)
            && work_state.running_instance_id() == Some(self.instance_id);
        !pending_matches && !running_matches
    }

    fn depends_on(self, pool_key: usize) -> bool {
        self.pool_key == pool_key
    }
}

pub(crate) fn cancel_pending_from_binding(
    binding: WorkQueuePoolBinding,
    work: &ScheduledWork,
    wait_running: bool,
) -> PendingCancel {
    let owner = binding.owner();
    match binding.cancel_pending(work, wait_running) {
        WorkQueuePoolPendingCancel::Retry => PendingCancel::Retry,
        WorkQueuePoolPendingCancel::Done(done) => {
            let (result, removed, waiters) = pending_cancel_done(owner, done);
            PendingCancel::Done(result, removed, waiters)
        }
    }
}

fn pending_cancel_done(
    owner: QueueOwner,
    done: WorkQueuePoolPendingCancelDone,
) -> (CancelWorkResult, Option<ScheduledWork>, DeferredWake) {
    let WorkQueuePoolPendingCancelDone {
        result,
        removed,
        work_done,
        work_state_change,
        queue_completion,
        barriers,
        pool_wake,
    } = done;
    let mut waiters = DeferredWake::default().with_barriers(complete_barriers_defer_wake(barriers));
    if let Some(work_done) = work_done {
        waiters = waiters.with_work(work_done);
    }
    if let Some(work_state_change) = work_state_change {
        waiters = waiters.with_work(work_state_change);
    }
    if let Some(completion) = queue_completion {
        waiters = waiters.with_queue(owner.queue().collect_waiters(completion));
    }
    if let Some(wake) = pool_wake {
        wake.execute();
    }
    (result, removed, waiters)
}

pub(crate) fn finish_workqueue_pool_enqueue(outcome: WorkQueuePoolEnqueue) -> QueueWorkResult {
    match outcome {
        WorkQueuePoolEnqueue::Queued(queued) => {
            queued.pool_wake.execute();
            let _ = queued.work_state_change.wake();
            QueueWorkResult::Queued
        }
        WorkQueuePoolEnqueue::Rejected(result) => result,
    }
}

pub(crate) fn process_one_work_with_token(
    binding: WorkQueuePoolBinding,
    worker_id: WorkerId,
    worker_token: WorkerExecutionToken,
    work: ScheduledWork,
) {
    let owner = binding.owner();
    let queue = owner.queue();
    let pool_key = binding.pool_key();
    {
        let _current = CurrentWorkGuard::enter(queue, pool_key, worker_id, worker_token, &work);
        work.run();
    }
    finish_work_with_pwq(Some(&binding), queue, &work);
}

pub(crate) fn process_one_pool_work(binding: SystemPoolBinding, worker_id: WorkerId) -> bool {
    let task_pool = TaskPoolBinding::from_system_binding(binding);
    let outcome = WorkQueuePoolBinding::take_any_runnable_from_task_pool(task_pool, worker_id);
    let queue_name = outcome
        .binding
        .as_ref()
        .map_or(binding.kind().queue_name(), |binding| {
            binding.owner().name()
        });
    let Some((work, work_binding, worker_token)) = finish_workqueue_pool_take(queue_name, outcome)
    else {
        return false;
    };
    process_one_work_with_token(work_binding, worker_id, worker_token, work);
    true
}

/// Runs one runnable work item from a bottom-half workerqueue pool.
///
/// The IRQ provider calls this from its softirq action. Unlike task-context
/// workers, BH execution has no task-local worker context and must not
/// participate in scheduler sleep accounting.
pub fn process_one_bottom_half_pool_work(binding: BottomHalfPoolBinding) -> bool {
    let worker_id = WorkerId::new(0);
    let outcome = WorkQueuePoolBinding::take_any_runnable_from_bottom_half_pool(binding, worker_id);
    let queue_name = outcome
        .binding
        .as_ref()
        .map_or(binding.kind().queue_name(), |binding| {
            binding.owner().name()
        });
    let Some((work, work_binding, _worker_token)) = finish_workqueue_pool_take(queue_name, outcome)
    else {
        return false;
    };
    process_one_bottom_half_work(work_binding, work);
    true
}

fn process_one_bottom_half_work(binding: WorkQueuePoolBinding, work: ScheduledWork) {
    let owner = binding.owner();
    let queue = owner.queue();
    work.run();
    finish_work_with_pwq(Some(&binding), queue, &work);
}

fn finish_workqueue_pool_take(
    queue_name: &'static str,
    outcome: WorkQueuePoolRunnableTake,
) -> Option<(ScheduledWork, WorkQueuePoolBinding, WorkerExecutionToken)> {
    let completed_barriers = complete_barriers_defer_wake(outcome.completed_barriers);
    DeferredWake::default()
        .with_barriers(completed_barriers)
        .wake();
    for queue_wake in outcome.queue_wakes {
        queue_wake.wake();
    }
    for status in outcome.stale_statuses {
        warn!("workerqueue {queue_name} dropped stale pool entry with state {status:?}");
    }

    Some((outcome.work?, outcome.binding?, outcome.worker_token?))
}

#[cfg(unittest)]
pub(crate) fn finish_work(queue: &WorkQueue, work: &ScheduledWork) {
    finish_work_with_pwq(None, queue, work);
}

fn finish_work_with_pwq(
    binding_hint: Option<&WorkQueuePoolBinding>,
    queue: &WorkQueue,
    work: &ScheduledWork,
) {
    let finished = match work.finish_running_state() {
        Ok(finished) => finished,
        Err(other) => {
            warn!(
                "workerqueue {} finished work with unexpected state {:?}",
                queue.name(),
                other
            );
            return;
        }
    };
    let owner_waiters =
        finished
            .binding
            .as_ref()
            .map_or_else(DeferredWake::default, |running_binding| {
                let hinted_binding = binding_hint.filter(|binding| {
                    binding.pool_key() == finished.pool_key
                        && binding.owner().same_queue(running_binding.owner().queue())
                });
                match hinted_binding {
                    Some(binding) => finish_running_pool_work(RunningPoolWorkFinish {
                        binding,
                        pool_key: finished.pool_key,
                        color: finished.color,
                        worker_id: finished.worker_id,
                        worker_token: finished.worker_token,
                        work_key: work.key(),
                        instance_id: finished.instance_id,
                    }),
                    None => finish_running_binding(
                        running_binding,
                        finished.pool_key,
                        finished.color,
                        finished.worker_id,
                        finished.worker_token,
                        work.key(),
                        finished.instance_id,
                    ),
                }
            });
    let _ = finished.work_waiters.wake();
    owner_waiters.wake();
}

fn finish_running_binding(
    binding: &WorkQueuePoolBinding,
    pool_key: usize,
    color: WorkColor,
    worker_id: Option<WorkerId>,
    worker_token: Option<WorkerExecutionToken>,
    work_key: usize,
    instance_id: Option<WorkInstanceId>,
) -> DeferredWake {
    finish_running_pool_work(RunningPoolWorkFinish {
        binding,
        pool_key,
        color,
        worker_id,
        worker_token,
        work_key,
        instance_id,
    })
}

struct RunningPoolWorkFinish<'a> {
    binding: &'a WorkQueuePoolBinding,
    pool_key: usize,
    color: WorkColor,
    worker_id: Option<WorkerId>,
    worker_token: Option<WorkerExecutionToken>,
    work_key: usize,
    instance_id: Option<WorkInstanceId>,
}

fn finish_running_pool_work(request: RunningPoolWorkFinish<'_>) -> DeferredWake {
    let finish = request.binding.finish_running(
        request.pool_key,
        request.color,
        request.worker_id,
        request.work_key,
        request.instance_id,
        request.worker_token,
    );
    if let Some(wake) = finish.pool_wake {
        wake.execute();
    }
    DeferredWake::default()
        .with_barriers(complete_barriers_defer_wake(finish.running_barriers))
        .with_queue(
            request
                .binding
                .owner()
                .queue()
                .collect_waiters(finish.queue_completion),
        )
}

pub(crate) fn attach_flush_barrier(
    work: &ScheduledWork,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<Option<WorkBarrier>, WorkqueueError> {
    loop {
        enum AttachTarget {
            Queued(WorkQueuePoolBinding, WorkInstanceId),
            Running(WorkQueuePoolBinding, WorkerId, WorkInstanceId),
        }

        let target = {
            let work_state = work.inner().state.lock();
            let Some(target) = FlushTarget::from_work_state(&work_state) else {
                return Ok(None);
            };
            reject_worker_wait_deadlock(&work_state, target, worker_context)?;

            if matches!(work_state.status(), WorkStatus::Pending)
                && let Some(binding) = work_state.pending_binding_cloned()
            {
                AttachTarget::Queued(binding, target.instance_id)
            } else if matches!(work_state.status(), WorkStatus::Running)
                && let (Some(binding), Some(worker_id)) = (
                    work_state.running_binding_cloned(),
                    work_state.running_worker_id(),
                )
            {
                AttachTarget::Running(binding, worker_id, target.instance_id)
            } else {
                return Ok(None);
            }
        };

        match target {
            AttachTarget::Queued(binding, instance_id) => {
                match attach_queued_flush_barrier(work, binding, instance_id, worker_context)? {
                    WorkQueuePoolBarrierAttach::Attached(barrier) => return Ok(Some(barrier)),
                    WorkQueuePoolBarrierAttach::Gone => return Ok(None),
                    WorkQueuePoolBarrierAttach::Full => return Err(WorkqueueError::BarrierFull),
                    WorkQueuePoolBarrierAttach::Retry => continue,
                }
            }
            AttachTarget::Running(binding, worker_id, instance_id) => {
                match attach_running_flush_barrier(
                    work,
                    binding,
                    worker_id,
                    instance_id,
                    worker_context,
                )? {
                    WorkQueuePoolBarrierAttach::Attached(barrier) => return Ok(Some(barrier)),
                    WorkQueuePoolBarrierAttach::Full => return Err(WorkqueueError::BarrierFull),
                    WorkQueuePoolBarrierAttach::Gone | WorkQueuePoolBarrierAttach::Retry => {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

fn attach_running_flush_barrier(
    work: &ScheduledWork,
    binding: WorkQueuePoolBinding,
    worker_id: WorkerId,
    instance_id: WorkInstanceId,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError> {
    binding.attach_running_barrier_checked(work, worker_id, instance_id, |work_state| {
        recheck_running_flush_target(work_state, &binding, worker_id, instance_id, worker_context)
    })
}

fn recheck_running_flush_target(
    work_state: &WorkState,
    binding: &WorkQueuePoolBinding,
    worker_id: WorkerId,
    instance_id: WorkInstanceId,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<bool, WorkqueueError> {
    let Some(target) = FlushTarget::from_work_state(work_state) else {
        return Ok(false);
    };
    if target.instance_id != instance_id {
        return Ok(false);
    }
    reject_worker_wait_deadlock(work_state, target, worker_context)?;
    let is_same_running_instance = matches!(work_state.status(), WorkStatus::Running)
        && work_state.running_instance_id() == Some(instance_id)
        && work_state.running_pool_key() == binding.pool_key()
        && work_state.running_worker_id() == Some(worker_id)
        && work_state
            .running_binding_cloned()
            .is_some_and(|running_binding| running_binding.same_binding(binding));
    if is_same_running_instance {
        Ok(true)
    } else {
        Ok(false)
    }
}

fn attach_queued_flush_barrier(
    work: &ScheduledWork,
    binding: WorkQueuePoolBinding,
    instance_id: WorkInstanceId,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError> {
    binding.attach_pending_barrier_checked(work, instance_id, |work_state| {
        prepare_queued_flush_barrier_locked(work_state, &binding, instance_id, worker_context)
    })
}

fn prepare_queued_flush_barrier_locked(
    work_state: &mut WorkState,
    binding: &WorkQueuePoolBinding,
    instance_id: WorkInstanceId,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError> {
    let Some(target) = FlushTarget::from_work_state(work_state) else {
        return Ok(WorkQueuePoolBarrierAttach::Gone);
    };
    if target.instance_id != instance_id {
        return Ok(WorkQueuePoolBarrierAttach::Gone);
    }
    reject_worker_wait_deadlock(work_state, target, worker_context)?;
    if !matches!(work_state.status(), WorkStatus::Pending)
        || !work_state.pending_binding_is(binding)
    {
        return Ok(WorkQueuePoolBarrierAttach::Retry);
    }

    let barrier = WorkBarrier::new();
    Ok(WorkQueuePoolBarrierAttach::Attached(barrier))
}

pub(crate) fn wait_for_workqueue_idle(queue: &WorkQueueHandle) -> Result<(), WorkqueueError> {
    let sync = queue.queue().sync();
    loop {
        if queue.queue().is_idle() {
            return Ok(());
        }
        sync.idle_completion().reinit();
        if queue.queue().is_idle() {
            return Ok(());
        }
        WorkqueueSyncWaitIf::wait_for_completion(sync.idle_completion())
            .map_err(|_| WorkqueueError::WaitFailed)?;
    }
}

pub(crate) fn reject_invalid_wait_context() -> Result<(), WorkqueueError> {
    if WorkqueueContextIf::is_invalid_wait_context() {
        return Err(WorkqueueError::InvalidContext);
    }
    Ok(())
}

pub(crate) fn reject_self_wait(work: &ScheduledWork) -> Result<(), WorkqueueError> {
    if WorkqueueTaskContextIf::current_work_context()
        .is_some_and(|context| context.work_key() == work.key())
    {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

pub(crate) fn reject_worker_pool_wait_deadlock(pool_key: usize) -> Result<(), WorkqueueError> {
    if WorkqueueTaskContextIf::current_work_context()
        .is_some_and(|context| context.pool_key() == pool_key)
    {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

pub(crate) fn queue_result_to_wait_error(result: QueueWorkResult) -> WorkqueueError {
    match result {
        QueueWorkResult::InvalidCpu | QueueWorkResult::WorkerUnavailable => {
            WorkqueueError::WorkerUnavailable
        }
        QueueWorkResult::Disabled => WorkqueueError::WorkerUnavailable,
        QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued | QueueWorkResult::QueueFull => {
            WorkqueueError::WaitFailed
        }
    }
}

pub(crate) fn reject_delayed_target_wait_deadlock(
    target: &DelayedWorkTarget,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<(), WorkqueueError> {
    if let Some(context) = worker_context
        && target
            .pool_key()
            .is_some_and(|target_pool_key| target_pool_key == context.pool_key())
    {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

pub(crate) fn reject_worker_wait_deadlock(
    work_state: &WorkState,
    target: FlushTarget,
    worker_context: Option<WorkqueueTaskContext>,
) -> Result<(), WorkqueueError> {
    let Some(context) = worker_context else {
        return Ok(());
    };

    if target.depends_on(context.pool_key()) && !target.is_gone(work_state) {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

pub(crate) struct CurrentWorkGuard {
    context: WorkqueueTaskContext,
    previous_context: Option<WorkqueueTaskContext>,
}

impl CurrentWorkGuard {
    pub(crate) fn enter(
        queue: &WorkQueue,
        pool_key: usize,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
        work: &ScheduledWork,
    ) -> Self {
        let context =
            WorkqueueTaskContext::new(work.key(), queue.key(), pool_key, worker_id, worker_token);
        let previous_context = WorkqueueTaskContextIf::set_current_work_context(context);
        if previous_context.is_some() {
            warn!("nested workerqueue callback entered");
        }
        Self {
            context,
            previous_context,
        }
    }
}

impl Drop for CurrentWorkGuard {
    fn drop(&mut self) {
        if WorkqueueTaskContextIf::current_work_context() == Some(self.context) {
            if let Some(previous_context) = self.previous_context {
                let replaced = WorkqueueTaskContextIf::set_current_work_context(previous_context);
                if replaced != Some(self.context) {
                    warn!("workerqueue current-work state changed during callback");
                }
            } else if !WorkqueueTaskContextIf::clear_current_work_context(self.context) {
                warn!("workerqueue current-work state changed during callback");
            }
        } else {
            warn!("workerqueue current-work state changed during callback");
        }
    }
}
