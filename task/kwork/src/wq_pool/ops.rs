// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{
    accounting::WorkQueuePoolAccountingCommit,
    binding::ExecutionPoolBinding,
    handle::WorkQueuePoolBinding,
    outcome::{
        WorkQueuePoolBarrierAttach, WorkQueuePoolEnqueue, WorkQueuePoolPendingCancel,
        WorkQueuePoolPendingCancelDone, WorkQueuePoolRunnableTake, WorkQueuePoolRunningFinish,
    },
};
use crate::{
    BottomHalfPoolBinding, CancelWorkResult, MAX_SYSTEM_WORKERS_PER_CPU, PendingBarrierAttach,
    QueueWorkResult, ScheduledWork, TaskPoolBinding, WorkBarrier, WorkColor, WorkEntry,
    WorkInstanceId, WorkState, WorkStatus, WorkerExecutionToken, WorkerId, WorkqueueError,
};

impl WorkQueuePoolBinding {
    pub(crate) fn cancel_pending(
        &self,
        work: &ScheduledWork,
        wait_running: bool,
    ) -> WorkQueuePoolPendingCancel {
        self.cancel_pending_from_pool(work, wait_running)
    }

    fn take_any_runnable_from_pool(
        binding: ExecutionPoolBinding,
        worker_id: WorkerId,
    ) -> WorkQueuePoolRunnableTake {
        let pool = binding.pool();
        let cpu_id = binding.cpu_id();
        let mut pool_state = pool.state.lock();
        let outcome = pool_state.take_any_runnable_work(pool.key(), worker_id);
        let took_work = outcome.work.is_some();
        if let Some(binding) = outcome.binding.as_ref()
            && binding.binding.cpu_id() == cpu_id
        {
            binding.state().lock().start_running();
        }
        let mut stale_statuses = alloc::vec::Vec::new();
        let mut queue_wakes = alloc::vec::Vec::new();
        for stale in outcome.stale_entries {
            let stale_binding = WorkQueuePoolBinding::resolved(stale.owner, binding);
            if stale_binding.binding.cpu_id() == cpu_id {
                let mut binding_state = stale_binding.state().lock();
                pool_state
                    .discard_active_entry_locked(&mut binding_state, stale_binding.binding_key());
                if stale.barrier_count != 0 {
                    let completion = binding_state
                        .complete_work_and_linked_barriers(stale.color, stale.barrier_count);
                    queue_wakes.push(stale_binding.owner().queue().collect_waiters(completion));
                }
            }
            stale_statuses.push(stale.status);
        }
        pool.update_runnable_stats(&pool_state);
        if took_work {
            pool.note_progress();
        }
        WorkQueuePoolRunnableTake {
            work: outcome.work,
            binding: outcome.binding,
            worker_token: outcome.worker_token,
            completed_barriers: outcome.completed_barriers,
            queue_wakes,
            stale_statuses,
        }
    }

    pub(crate) fn take_any_runnable_from_task_pool(
        binding: TaskPoolBinding,
        worker_id: WorkerId,
    ) -> WorkQueuePoolRunnableTake {
        Self::take_any_runnable_from_pool(ExecutionPoolBinding::Task(binding), worker_id)
    }

    pub(crate) fn take_any_runnable_from_bottom_half_pool(
        binding: BottomHalfPoolBinding,
        worker_id: WorkerId,
    ) -> WorkQueuePoolRunnableTake {
        Self::take_any_runnable_from_pool(ExecutionPoolBinding::BottomHalf(binding), worker_id)
    }

    pub(crate) fn finish_running(
        &self,
        pool_key: usize,
        color: WorkColor,
        worker_id: Option<WorkerId>,
        work_key: usize,
        instance_id: Option<WorkInstanceId>,
        worker_token: Option<WorkerExecutionToken>,
    ) -> WorkQueuePoolRunningFinish {
        let mut pool_state = self.binding.pool().state.lock();
        let running_barriers = match (
            worker_id.filter(|worker_id| {
                pool_key == self.binding.pool_key()
                    && worker_id.as_usize() < MAX_SYSTEM_WORKERS_PER_CPU
            }),
            instance_id,
            worker_token,
        ) {
            (Some(worker_id), Some(instance_id), Some(worker_token)) => pool_state
                .finish_running_work(worker_id.as_usize(), work_key, instance_id, worker_token),
            _ => alloc::vec::Vec::new(),
        };
        let mut binding_state = self.state().lock();
        pool_state.finish_active_entry_locked(&mut binding_state, self.binding_key());
        let queue_completion =
            binding_state.complete_work_and_linked_barriers(color, running_barriers.len());
        let wake_plan = pool_state.select_worker_to_kick();
        self.binding.pool().update_runnable_stats(&pool_state);
        self.binding.pool().note_progress();
        WorkQueuePoolRunningFinish {
            queue_completion,
            running_barriers,
            pool_wake: Some(self.binding.wake(wake_plan)),
        }
    }

    pub(crate) fn attach_pending_barrier_checked(
        &self,
        work: &ScheduledWork,
        instance_id: WorkInstanceId,
        prepare: impl FnOnce(&mut WorkState) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError>,
    ) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError> {
        let mut pool_state = self.binding.pool().state.lock();
        let mut binding_state = self.state().lock();
        let mut work_state = work.inner().state.lock();
        let attach = prepare(&mut work_state)?;
        let color = work_state.pending_color();
        let attach = self.finish_pending_barrier_attach(attach, instance_id, |barrier| {
            pool_state.attach_pending_barrier(work, self.binding_key(), barrier)
        });
        if matches!(attach, WorkQueuePoolBarrierAttach::Attached(_)) {
            binding_state.inc_in_flight(color);
        }
        Ok(attach)
    }

    pub(crate) fn attach_running_barrier_checked(
        &self,
        work: &ScheduledWork,
        worker_id: WorkerId,
        instance_id: WorkInstanceId,
        prepare: impl FnOnce(&WorkState) -> Result<bool, WorkqueueError>,
    ) -> Result<WorkQueuePoolBarrierAttach, WorkqueueError> {
        let mut pool_state = self.binding.pool().state.lock();
        let mut binding_state = self.state().lock();
        let work_state = work.inner().state.lock();
        if !prepare(&work_state)? {
            return Ok(WorkQueuePoolBarrierAttach::Gone);
        }
        let color = work_state.running_color();
        let barrier = WorkBarrier::new();
        match pool_state.attach_running_barrier(worker_id, work.key(), instance_id, barrier.clone())
        {
            PendingBarrierAttach::Attached => {
                binding_state.inc_in_flight(color);
                Ok(WorkQueuePoolBarrierAttach::Attached(barrier))
            }
            PendingBarrierAttach::Full => Ok(WorkQueuePoolBarrierAttach::Full),
            PendingBarrierAttach::Missing => {
                warn!(
                    "system worker slot for work instance {:?} disappeared during flush",
                    instance_id
                );
                Ok(WorkQueuePoolBarrierAttach::Gone)
            }
        }
    }

    fn finish_pending_barrier_attach(
        &self,
        attach: WorkQueuePoolBarrierAttach,
        instance_id: WorkInstanceId,
        attach_fn: impl FnOnce(WorkBarrier) -> PendingBarrierAttach,
    ) -> WorkQueuePoolBarrierAttach {
        let WorkQueuePoolBarrierAttach::Attached(barrier) = attach else {
            return attach;
        };
        match attach_fn(barrier.clone()) {
            PendingBarrierAttach::Attached => WorkQueuePoolBarrierAttach::Attached(barrier),
            PendingBarrierAttach::Full => WorkQueuePoolBarrierAttach::Full,
            PendingBarrierAttach::Missing => {
                warn!(
                    "queued work instance {:?} not found in workerqueue {} during flush",
                    instance_id,
                    self.owner().name()
                );
                WorkQueuePoolBarrierAttach::Gone
            }
        }
    }

    pub(crate) fn queue_work(&self, work: &ScheduledWork) -> WorkQueuePoolEnqueue {
        let queue = self.owner.queue();
        if let Err(result) = queue.reject_queue_if_destroying() {
            return WorkQueuePoolEnqueue::rejected(result);
        }
        if let Err(result) = work.reject_queue_if_disabled() {
            return WorkQueuePoolEnqueue::rejected(result);
        }
        let mut pool_state = self.binding.pool().state.lock();
        let mut binding_state = self.state().lock();
        let accounting =
            WorkQueuePoolAccountingCommit::capture(&binding_state, binding_state.is_idle());
        let is_active = binding_state.can_activate();
        match work.queue_new_pending_with(self.clone(), accounting.color(), |instance_id| {
            let entry = WorkEntry::new(work.clone(), self.owner(), accounting.color(), instance_id);
            let pending_result = if is_active {
                pool_state.pending.push_runnable(entry)
            } else {
                pool_state.pending.push_inactive(entry)
            };
            pending_result.map_err(|_| QueueWorkResult::QueueFull)
        }) {
            Ok(_instance_id) => {}
            Err(result) => {
                return WorkQueuePoolEnqueue::rejected(result);
            }
        };
        if is_active {
            binding_state.add_active();
            pool_state.runnable_count += 1;
        }
        if accounting.commit(&mut binding_state) {
            queue.reinit_idle_waiters_if_initialized();
        }
        let wake_plan = pool_state.select_worker_to_kick();
        self.binding.pool().update_runnable_stats(&pool_state);
        WorkQueuePoolEnqueue::queued(work, self.binding.wake(wake_plan))
    }

    fn cancel_pending_from_pool(
        &self,
        work: &ScheduledWork,
        _wait_running: bool,
    ) -> WorkQueuePoolPendingCancel {
        let binding_key = self.binding_key();
        let mut pool_state = self.binding.pool().state.lock();
        let mut binding_state = self.state().lock();
        let mut work_state = work.inner().state.lock();

        match work_state.status() {
            WorkStatus::Pending if work_state.pending_binding_is(self) => {
                let color = work_state.pending_color();
                let mut removed = pool_state.remove_pending_work(work, binding_key);
                let mut barriers = alloc::vec::Vec::new();
                if let Some(entry) = removed.entry_mut() {
                    barriers.append(&mut entry.take_barriers());
                }
                let _activated =
                    pool_state.remove_active_entry_locked(&mut binding_state, &removed);
                let wake_plan = pool_state.select_worker_to_kick();
                self.binding.pool().update_runnable_stats(&pool_state);
                let completion =
                    binding_state.complete_work_and_linked_barriers(color, barriers.len());
                work_state.set_idle();
                let has_removed = removed.entry().is_some();
                if !has_removed {
                    warn!(
                        "pending work not found in {:?} pool for CPU {} during cancel",
                        self.binding.pool_key(),
                        self.binding.cpu_id().as_usize()
                    );
                }
                let result = if has_removed {
                    CancelWorkResult::CancelledPending
                } else {
                    CancelWorkResult::NotPending
                };
                WorkQueuePoolPendingCancel::Done(
                    WorkQueuePoolPendingCancelDone::new(result)
                        .with_removed(removed.into_work())
                        .with_work_done(work.complete_done_defer_wake())
                        .with_queue_completion(completion)
                        .with_barriers(barriers)
                        .with_pool_wake(self.binding.wake(wake_plan)),
                )
            }
            WorkStatus::Pending => WorkQueuePoolPendingCancel::Retry,
            WorkStatus::Running => WorkQueuePoolPendingCancel::Done(
                WorkQueuePoolPendingCancelDone::new(CancelWorkResult::Running),
            ),
            WorkStatus::Idle | WorkStatus::DelayedPending => WorkQueuePoolPendingCancel::Done(
                WorkQueuePoolPendingCancelDone::new(CancelWorkResult::NotPending),
            ),
        }
    }

    pub(crate) fn queue_reserved_delayed_work(
        &self,
        work: &ScheduledWork,
        instance_id: WorkInstanceId,
    ) -> WorkQueuePoolEnqueue {
        let queue = self.owner.queue();
        if let Err(result) = queue.reject_queue_if_destroying() {
            return WorkQueuePoolEnqueue::rejected(result);
        }
        if let Err(result) = work.reject_queue_if_disabled() {
            return WorkQueuePoolEnqueue::rejected(result);
        }
        let mut pool_state = self.binding.pool().state.lock();
        let mut binding_state = self.state().lock();
        let accounting =
            WorkQueuePoolAccountingCommit::capture(&binding_state, binding_state.is_idle());
        let is_active = binding_state.can_activate();
        match work.queue_reserved_pending_with(
            instance_id,
            self.clone(),
            accounting.color(),
            || {
                let entry =
                    WorkEntry::new(work.clone(), self.owner(), accounting.color(), instance_id);
                let pending_result = if is_active {
                    pool_state.pending.push_runnable(entry)
                } else {
                    pool_state.pending.push_inactive(entry)
                };
                pending_result.map_err(|_| QueueWorkResult::QueueFull)
            },
        ) {
            Ok(()) => {}
            Err(result) => return WorkQueuePoolEnqueue::rejected(result),
        };
        if is_active {
            binding_state.add_active();
            pool_state.runnable_count += 1;
        }
        if accounting.commit(&mut binding_state) {
            queue.reinit_idle_waiters_if_initialized();
        }
        let wake_plan = pool_state.select_worker_to_kick();
        self.binding.pool().update_runnable_stats(&pool_state);
        WorkQueuePoolEnqueue::queued(work, self.binding.wake(wake_plan))
    }
}
