// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core worker-pool state machine.

use ktime_types::MonotonicInstant;

use crate::{
    ActionBatch, EntryKey, EntryOwner, EntryPayload, ImmediateAction, PoolEntry, PoolId,
    QueueRemoveResult, WorkerExecutionToken, WorkerId, WorkerPoolPolicy, WorkerState,
    action::ManagementAction,
    queue::PoolRunQueue,
    worker::{ExecutionAccounting, Worker},
};

/// Worker-pool operation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPoolError {
    /// Worker id is outside the configured slot table.
    InvalidWorker,
    /// Worker state does not allow the requested transition.
    InvalidWorkerState,
    /// Queue storage is full; the entry is returned to the caller.
    QueueFull(PoolEntry),
}

/// Worker park decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkDecision {
    /// Runnable entries are available; the worker should try taking one.
    Run,
    /// No runnable entries are available; the worker should block or park.
    Wait,
    /// The worker should exit in its own context.
    Exit,
}

/// Result returned by an external runnable-entry claimer.
#[derive(Debug, Eq, PartialEq)]
pub enum RunnableClaim<A, S> {
    /// The entry was claimed and can run.
    Run(A),
    /// The entry no longer matches external state and was discarded.
    Stale(S),
}

/// Runnable candidate reserved for lock-free external claim validation.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnableCandidate {
    /// Popped runnable entry.
    pub entry: PoolEntry,
    /// Execution token reserved for this candidate.
    pub token: WorkerExecutionToken,
    /// Pool actions produced while dispatching.
    pub actions: ActionBatch,
}

/// Result of trying to prepare one runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub enum RunnableCandidateResult {
    /// A candidate was popped and reserved for external claim.
    Candidate(RunnableCandidate),
    /// No runnable entry is available for this worker.
    Empty(ActionBatch),
}

/// Result of discarding a stale runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnableCandidateDiscard {
    /// Whether the worker should immediately try another runnable entry.
    pub should_retry: bool,
    /// Pool actions produced while discarding the candidate.
    pub actions: ActionBatch,
}

/// External claim hook for opaque runnable entries.
///
/// The worker pool owns worker dispatch and runnable FIFO mechanics. The user
/// owns cross-object state and decides whether a popped opaque entry can still
/// run.
///
/// `claim` is called after the worker pool has dropped its lock. It may take
/// user-owned locks to validate that the popped entry still matches external
/// state, but it must not call back into the same worker pool before returning
/// a claim result.
pub trait RunnableClaimer {
    /// User value returned when the entry is claimed for execution.
    type Accepted;
    /// User value describing why a popped entry was stale.
    type Stale;

    /// Attempts to claim `entry` for `worker` using the reserved execution
    /// `token`.
    fn claim(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        entry: PoolEntry,
    ) -> RunnableClaim<Self::Accepted, Self::Stale>;

    /// Records a stale popped entry.
    fn record_stale(&mut self, stale: Self::Stale);
}

/// Completion result for management slow paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementComplete {
    /// A runtime worker was created.
    Spawned,
    /// Runtime worker creation failed.
    SpawnFailed,
}

/// Worker-pool instance.
///
/// This type is a pure state container. The eventual runtime integration must
/// protect it with the pool-instance lock and execute returned actions only
/// after dropping that lock.
pub struct WorkerPool<ThreadRef, const MAX_WORKERS: usize, const ENTRY_CAP: usize> {
    id: PoolId,
    policy: WorkerPoolPolicy,
    workers: [Worker<ThreadRef>; MAX_WORKERS],
    counts: WorkerCounts,
    queue: PoolRunQueue<ENTRY_CAP>,
    management_pending: bool,
    create_retry_after: Option<MonotonicInstant>,
}

/// Read-only worker-pool state snapshot for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolSnapshot<const MAX_WORKERS: usize> {
    pub installed_workers: usize,
    pub nr_creating: usize,
    pub nr_idle: usize,
    pub nr_preparing: usize,
    pub nr_claiming: usize,
    pub nr_running_state: usize,
    pub nr_sleeping: usize,
    pub nr_retire_requested: usize,
    pub nr_exiting: usize,
    pub nr_concurrency: usize,
    pub runnable: usize,
    pub deferred: usize,
    pub worker_states: [WorkerState; MAX_WORKERS],
}

impl<ThreadRef, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
    WorkerPool<ThreadRef, MAX_WORKERS, ENTRY_CAP>
{
    /// Creates an empty pool instance with `policy`.
    pub const fn new(id: PoolId, policy: WorkerPoolPolicy) -> Self {
        Self {
            id,
            policy,
            workers: [const { Worker::new() }; MAX_WORKERS],
            counts: WorkerCounts::new(),
            queue: PoolRunQueue::new(),
            management_pending: false,
            create_retry_after: None,
        }
    }

    /// Returns this pool identifier.
    pub const fn id(&self) -> PoolId {
        self.id
    }

    /// Configures the identity and policy before runtime workers are
    /// installed.
    ///
    /// Runtime static pool tables often need `const` construction before their
    /// CPU-local identity is known. Reconfiguration is accepted only while the
    /// pool is completely empty.
    pub fn configure_empty(&mut self, id: PoolId, policy: WorkerPoolPolicy) -> bool {
        if self.counts != WorkerCounts::new() || !self.queue.is_empty() {
            return false;
        }
        self.id = id;
        self.policy = policy;
        true
    }

    /// Returns a worker's lifecycle state.
    pub fn worker_state(&self, worker: WorkerId) -> Option<WorkerState> {
        self.workers.get(worker.as_usize()).map(|w| w.state)
    }

    /// Returns the current runnable FIFO length.
    pub fn runnable_len(&self) -> usize {
        self.queue.runnable_len()
    }

    /// Returns a read-only state snapshot for diagnostics.
    pub fn snapshot(&self) -> WorkerPoolSnapshot<MAX_WORKERS> {
        let mut worker_states = [WorkerState::Empty; MAX_WORKERS];
        let mut index = 0usize;
        while index < MAX_WORKERS {
            worker_states[index] = self.workers[index].state;
            index += 1;
        }
        WorkerPoolSnapshot {
            installed_workers: self.counts.installed_workers,
            nr_creating: self.counts.nr_creating,
            nr_idle: self.counts.nr_idle,
            nr_preparing: self.counts.nr_preparing,
            nr_claiming: self.counts.nr_claiming,
            nr_running_state: self.counts.nr_running_state,
            nr_sleeping: self.counts.nr_sleeping,
            nr_retire_requested: self.counts.nr_retire_requested,
            nr_exiting: self.counts.nr_exiting,
            nr_concurrency: self.counts.nr_concurrency,
            runnable: self.queue.runnable_len(),
            deferred: self.queue.deferred_len(),
            worker_states,
        }
    }

    /// Returns total queued entries across runnable and deferred lanes.
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Returns runnable entries for one owner.
    pub fn runnable_len_for_owner(&self, owner: EntryOwner) -> usize {
        self.queue.runnable_len_for_owner(owner)
    }

    /// Returns queued entries for one owner across runnable and deferred lanes.
    pub fn queued_len_for_owner(&self, owner: EntryOwner) -> usize {
        self.queue.runnable_len_for_owner(owner) + self.queue.deferred_len_for_owner(owner)
    }

    /// Returns the number of installed runtime workers.
    pub fn installed_workers(&self) -> usize {
        self.counts.installed_workers
    }

    /// Returns the number of workers currently counted for concurrency.
    pub fn nr_concurrency(&self) -> usize {
        self.counts.nr_concurrency
    }

    /// Installs a runtime worker slot.
    pub fn install_worker(
        &mut self,
        worker: WorkerId,
        thread_ref: ThreadRef,
    ) -> Result<(), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if !matches!(
            self.workers[index].state,
            WorkerState::Empty | WorkerState::Creating
        ) {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.workers[index].set_thread_ref(thread_ref);
        self.transition(index, WorkerState::Idle);
        Ok(())
    }

    /// Returns the thread reference attached to one installed worker.
    pub fn worker_thread_ref(&self, worker: WorkerId) -> Option<&ThreadRef> {
        let index = self.checked_worker(worker).ok()?;
        self.workers[index].thread_ref.as_ref()
    }

    /// Enqueues an entry directly into the runnable FIFO and evaluates dispatch.
    pub fn enqueue_runnable(
        &mut self,
        entry: PoolEntry,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        self.queue
            .push_runnable(entry)
            .map_err(WorkerPoolError::QueueFull)?;
        Ok(self.evaluate_dispatch(now))
    }

    /// Enqueues an entry into its owner's deferred lane.
    pub fn enqueue_deferred(&mut self, entry: PoolEntry) -> Result<(), WorkerPoolError> {
        self.queue
            .push_deferred(entry)
            .map_err(WorkerPoolError::QueueFull)
    }

    /// Promotes up to `budget` deferred entries for `owner` into runnable FIFO.
    pub fn promote_deferred(
        &mut self,
        owner: EntryOwner,
        budget: usize,
        now: MonotonicInstant,
    ) -> (usize, ActionBatch) {
        let promoted = self.queue.promote_deferred(owner, budget);
        let actions = if promoted == 0 {
            ActionBatch::new()
        } else {
            self.evaluate_dispatch(now)
        };
        (promoted, actions)
    }

    /// Promotes one deferred entry for `owner` and evaluates dispatch.
    pub fn promote_one_deferred(
        &mut self,
        owner: EntryOwner,
        now: MonotonicInstant,
    ) -> Option<(PoolEntry, ActionBatch)> {
        let entry = self.queue.promote_one_deferred(owner)?;
        Some((entry, self.evaluate_dispatch(now)))
    }

    /// Moves all runnable entries for `owner` into that owner's deferred lane.
    pub fn defer_runnable_for_owner(&mut self, owner: EntryOwner) -> usize {
        self.queue.defer_runnable_for_owner(owner)
    }

    /// Removes an entry from runnable or deferred queues.
    pub fn remove_entry(
        &mut self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<(PoolEntry, QueueRemoveResult)> {
        self.queue.remove(owner, key)
    }

    /// Returns a mutable payload reference by owner and key.
    pub fn get_payload_mut(
        &mut self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<&mut EntryPayload> {
        self.queue.get_mut(owner, key)
    }

    /// Pops the oldest runnable entry and reserves it for external claim.
    pub fn prepare_runnable_candidate(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<RunnableCandidateResult, WorkerPoolError> {
        let mut actions = ActionBatch::new();
        let index = self.checked_worker(worker)?;
        if self.queue.runnable_len() == 0 {
            if matches!(
                self.workers[index].state,
                WorkerState::Preparing | WorkerState::Idle
            ) {
                if self.mark_idle_for_wait(index, now) {
                    actions.append(self.notify_manager_for_idle_retire_if_needed(index));
                }
                return Ok(RunnableCandidateResult::Empty(actions));
            }
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.workers[index].state == WorkerState::Idle {
            actions.append(self.evaluate_dispatch(now));
        }
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }

        match self.queue.pop_runnable() {
            Some(entry) => {
                let token = self.workers[index].reserve_execution_token();
                self.transition(index, WorkerState::Claiming);
                Ok(RunnableCandidateResult::Candidate(RunnableCandidate {
                    entry,
                    token,
                    actions,
                }))
            }
            None => {
                self.mark_idle_for_wait(index, now);
                actions.append(self.notify_manager_for_idle_retire_if_needed(index));
                Ok(RunnableCandidateResult::Empty(actions))
            }
        }
    }

    /// Marks a claimed runnable candidate as running.
    pub fn commit_runnable_candidate(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Claiming
            || !self.workers[index].reserved_token_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        let current = self.workers[index]
            .start_reserved_execution(token, now)
            .expect("claiming worker token was checked before commit");
        self.transition(index, WorkerState::Running);
        self.counts.nr_concurrency += 1;
        let mut actions = ActionBatch::new();
        if let Some(deadline) = current
            .started_at
            .checked_add(self.policy.cpu_intensive_threshold())
        {
            actions.push_immediate(ImmediateAction::ArmCpuIntensiveTimer {
                pool: self.id,
                deadline,
            });
        }
        Ok(actions)
    }

    /// Discards a stale runnable candidate after external claim rejection.
    pub fn discard_runnable_candidate(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<RunnableCandidateDiscard, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Claiming
            || !self.workers[index].reserved_token_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.queue.runnable_len() != 0 {
            self.transition(index, WorkerState::Preparing);
            return Ok(RunnableCandidateDiscard {
                should_retry: true,
                actions: ActionBatch::new(),
            });
        }
        let became_idle = self.mark_idle_for_wait(index, now);
        Ok(RunnableCandidateDiscard {
            should_retry: false,
            actions: if became_idle {
                self.notify_manager_for_idle_retire_if_needed(index)
            } else {
                ActionBatch::new()
            },
        })
    }

    /// Marks a prepared worker as running after external claim succeeded.
    pub fn worker_started(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<(WorkerExecutionToken, ActionBatch), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Running);
        let current = self.workers[index].next_execution(now);
        self.counts.nr_concurrency += 1;
        let mut actions = ActionBatch::new();
        if let Some(deadline) = now.checked_add(self.policy.cpu_intensive_threshold()) {
            actions.push_immediate(ImmediateAction::ArmCpuIntensiveTimer {
                pool: self.id,
                deadline,
            });
        }
        Ok((current.token(), actions))
    }

    /// Finishes a worker execution and prepares the worker for more runnable
    /// work when possible.
    pub fn worker_finished(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        let state = self.workers[index].state;
        if !matches!(state, WorkerState::Running | WorkerState::Sleeping)
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if state == WorkerState::Running
            && self.workers[index]
                .current
                .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
        self.workers[index].clear_execution();
        if self.queue.runnable_len() != 0 {
            self.transition(index, WorkerState::Preparing);
            Ok(self.dispatch_worker_action(worker))
        } else {
            if self.mark_idle_for_wait(index, now) {
                Ok(self.notify_manager_for_idle_retire_if_needed(index))
            } else {
                Ok(ActionBatch::new())
            }
        }
    }

    /// Reports that a running worker blocked in a sleepable wait.
    pub fn worker_blocked(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Running
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.workers[index]
            .current
            .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
        self.transition(index, WorkerState::Sleeping);
        Ok(self.evaluate_dispatch(now))
    }

    /// Reports that a sleeping worker resumed.
    pub fn worker_resumed(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
    ) -> Result<(), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Sleeping
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Running);
        if self.workers[index]
            .current
            .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency += 1;
        }
        Ok(())
    }

    /// Accounts a worker tick and marks CPU-intensive executions.
    pub fn worker_tick(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        let Some(current) = self.workers[index].current else {
            return Err(WorkerPoolError::InvalidWorkerState);
        };
        if self.workers[index].state != WorkerState::Running
            || current.token() != token
            || current.accounting() == ExecutionAccounting::CpuIntensive
            || now.saturating_duration_since(current.started_at)
                < self.policy.cpu_intensive_threshold()
        {
            return Ok(ActionBatch::new());
        }
        self.workers[index].mark_cpu_intensive();
        self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        Ok(self.evaluate_dispatch(now))
    }

    /// Returns the CPU-intensive accounting deadline for a running execution.
    pub fn worker_tick_deadline(
        &self,
        worker: WorkerId,
        token: WorkerExecutionToken,
    ) -> Option<MonotonicInstant> {
        let index = self.checked_worker(worker).ok()?;
        let current = self.workers[index].current?;
        if self.workers[index].state != WorkerState::Running
            || current.token() != token
            || current.accounting() == ExecutionAccounting::CpuIntensive
        {
            return None;
        }
        current
            .started_at
            .checked_add(self.policy.cpu_intensive_threshold())
    }

    /// Returns what a worker at the wait boundary should do.
    pub fn worker_ready_to_park(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<(ParkDecision, ActionBatch), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        match self.workers[index].state {
            WorkerState::RetireRequested => {
                self.transition(index, WorkerState::Exiting);
                Ok((ParkDecision::Exit, ActionBatch::new()))
            }
            WorkerState::Preparing if self.queue.runnable_len() != 0 => {
                Ok((ParkDecision::Run, ActionBatch::new()))
            }
            WorkerState::Preparing | WorkerState::Idle => {
                let became_idle = self.mark_idle_for_wait(index, now);
                Ok((
                    ParkDecision::Wait,
                    if became_idle {
                        self.notify_manager_for_idle_retire_if_needed(index)
                    } else {
                        ActionBatch::new()
                    },
                ))
            }
            _ => Err(WorkerPoolError::InvalidWorkerState),
        }
    }

    /// Returns whether a parked worker should wake and re-enter the pool loop.
    pub fn worker_wait_ready(&self, worker: WorkerId) -> bool {
        let Ok(index) = self.checked_worker(worker) else {
            return true;
        };
        match self.workers[index].state {
            WorkerState::RetireRequested | WorkerState::Preparing => true,
            WorkerState::Idle => self.queue.runnable_len() != 0,
            _ => true,
        }
    }

    /// Completes worker exit after runtime-side task cleanup.
    pub fn worker_exit_complete(&mut self, worker: WorkerId) -> Result<(), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Exiting {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.workers[index].clear_execution();
        let _ = self.workers[index].clear_thread_ref();
        self.transition(index, WorkerState::Empty);
        Ok(())
    }

    /// Returns the next slow-path lifecycle action for the per-CPU manager.
    pub fn next_management_action(&mut self, now: MonotonicInstant) -> Option<ManagementAction> {
        self.management_pending = false;
        if self.can_spawn_worker(now)
            && let Some(index) = self.find_state(WorkerState::Empty)
        {
            self.transition(index, WorkerState::Creating);
            return Some(ManagementAction::SpawnWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
        }
        if self.policy.idle_retire()
            && let Some(index) = self.find_retirable_idle(now)
        {
            self.transition(index, WorkerState::RetireRequested);
            return Some(ManagementAction::RetireWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
        }
        None
    }

    /// Completes a spawn action.
    pub fn spawn_complete(
        &mut self,
        worker: WorkerId,
        result: ManagementComplete,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        match result {
            ManagementComplete::Spawned => {
                if self.workers[index].state != WorkerState::Idle {
                    return Err(WorkerPoolError::InvalidWorkerState);
                }
                self.create_retry_after = None;
            }
            ManagementComplete::SpawnFailed => {
                if self.workers[index].state != WorkerState::Creating {
                    return Err(WorkerPoolError::InvalidWorkerState);
                }
                self.transition(index, WorkerState::Empty);
                self.create_retry_after = now.checked_add(self.policy.create_retry_delay());
            }
        }
        Ok(self.evaluate_dispatch(now))
    }

    /// Re-evaluates dispatch after external queue policy made entries runnable.
    pub fn reevaluate_dispatch(&mut self, now: MonotonicInstant) -> ActionBatch {
        self.evaluate_dispatch(now)
    }

    /// Returns whether a manager slow path should run now.
    pub fn manager_should_run(&self, now: MonotonicInstant) -> bool {
        self.management_pending
            || self.can_spawn_worker(now)
            || (self.policy.idle_retire() && self.find_retirable_idle(now).is_some())
    }

    /// Returns the next time a manager slow path may become runnable without an
    /// external wake event.
    pub fn next_management_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant> {
        let mut next = None;

        if self.can_spawn_worker_after_retry()
            && let Some(deadline) = self.create_retry_after
            && deadline > now
        {
            next = Some(deadline);
        }

        if self.policy.idle_retire()
            && self.counts.installed_workers > self.policy.min_workers()
            && let Some(retire_after) = self.policy.idle_retire_after()
        {
            for worker in &self.workers {
                if worker.state != WorkerState::Idle {
                    continue;
                }
                let Some(idle_since) = worker.idle_since else {
                    continue;
                };
                if let Some(deadline) = idle_since.checked_add(retire_after)
                    && deadline > now
                {
                    next = Some(next.map_or(deadline, |current| current.min(deadline)));
                }
            }
        }

        next
    }

    /// Returns whether the current execution has CPU-intensive accounting.
    pub fn worker_is_cpu_intensive(&self, worker: WorkerId) -> bool {
        self.checked_worker(worker)
            .ok()
            .and_then(|index| self.workers[index].current)
            .is_some_and(|current| current.accounting() == ExecutionAccounting::CpuIntensive)
    }

    #[cfg(unittest)]
    pub fn set_cpu_intensive_threshold_for_tests(&mut self, threshold: ktime_types::TimeSpan) {
        self.policy.set_cpu_intensive_threshold_for_tests(threshold);
    }

    #[cfg(unittest)]
    pub fn set_create_retry_delay_for_tests(
        &mut self,
        delay: ktime_types::TimeSpan,
        now: MonotonicInstant,
    ) {
        self.policy.set_create_retry_delay_for_tests(delay);
        if self.create_retry_after.is_some() {
            self.create_retry_after = now.checked_add(delay);
        }
    }

    #[cfg(unittest)]
    pub fn cancel_preparing_for_tests(&mut self) {
        for index in 0..MAX_WORKERS {
            if matches!(
                self.workers[index].state,
                WorkerState::Preparing | WorkerState::Claiming
            ) {
                self.transition(index, WorkerState::Idle);
                self.workers[index].clear_execution();
            }
        }
    }

    #[cfg(unittest)]
    pub fn prepare_worker_for_tests(&mut self, worker: WorkerId) -> Result<(), WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        self.cancel_preparing_for_tests();
        if self.workers[index].state != WorkerState::Idle {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Preparing);
        Ok(())
    }

    #[cfg(unittest)]
    pub fn start_worker_for_tests(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<WorkerExecutionToken, WorkerPoolError> {
        let index = self.checked_worker(worker)?;
        match self.workers[index].state {
            WorkerState::Idle => self.transition(index, WorkerState::Preparing),
            WorkerState::Preparing => {}
            WorkerState::Running => {
                let token = self.workers[index].reserve_execution_token();
                let _ = self.workers[index].start_reserved_execution(token, now);
                return Ok(token);
            }
            _ => return Err(WorkerPoolError::InvalidWorkerState),
        }
        let token = self.workers[index].reserve_execution_token();
        self.transition(index, WorkerState::Running);
        if self.workers[index]
            .start_reserved_execution(token, now)
            .is_none()
        {
            self.transition(index, WorkerState::Preparing);
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.counts.nr_concurrency += 1;
        Ok(token)
    }

    fn checked_worker(&self, worker: WorkerId) -> Result<usize, WorkerPoolError> {
        let index = worker.as_usize();
        if index < MAX_WORKERS {
            Ok(index)
        } else {
            Err(WorkerPoolError::InvalidWorker)
        }
    }

    fn evaluate_dispatch(&mut self, now: MonotonicInstant) -> ActionBatch {
        let mut actions = ActionBatch::new();
        self.mark_cpu_intensive_workers(now);
        if self.queue.runnable_len() == 0 || self.counts.nr_concurrency != 0 {
            return actions;
        }

        if let Some(index) = self.find_state(WorkerState::Preparing) {
            actions.append(self.dispatch_worker_action(WorkerId::new(index)));
            return actions;
        }
        if let Some(index) = self.find_state(WorkerState::RetireRequested) {
            self.transition(index, WorkerState::Preparing);
            actions.append(self.dispatch_worker_action(WorkerId::new(index)));
            return actions;
        }
        if let Some(index) = self.find_state(WorkerState::Idle) {
            self.transition(index, WorkerState::Preparing);
            actions.append(self.dispatch_worker_action(WorkerId::new(index)));
            return actions;
        }
        if self.can_spawn_worker(now) {
            self.management_pending = true;
            actions.push_immediate(ImmediateAction::WakeManager { pool: self.id });
        }
        actions
    }

    fn mark_cpu_intensive_workers(&mut self, now: MonotonicInstant) {
        if self.queue.runnable_len() == 0 || self.counts.nr_concurrency == 0 {
            return;
        }
        for index in 0..MAX_WORKERS {
            if self.workers[index].state != WorkerState::Running {
                continue;
            }
            let Some(current) = self.workers[index].current else {
                continue;
            };
            if current.accounting() == ExecutionAccounting::CpuIntensive
                || now.saturating_duration_since(current.started_at)
                    < self.policy.cpu_intensive_threshold()
            {
                continue;
            }
            self.workers[index].mark_cpu_intensive();
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
    }

    fn can_spawn_worker(&self, now: MonotonicInstant) -> bool {
        self.can_spawn_worker_after_retry()
            && self
                .create_retry_after
                .is_none_or(|deadline| now >= deadline)
    }

    fn can_spawn_worker_after_retry(&self) -> bool {
        self.policy.dynamic_create()
            && self.policy.manager_managed()
            && self.queue.runnable_len() != 0
            && self.counts.nr_concurrency == 0
            && self.counts.nr_preparing == 0
            && self.counts.nr_retire_requested == 0
            && self.counts.nr_creating == 0
            && self.counts.installed_workers + self.counts.nr_creating < self.policy.max_workers()
    }

    fn find_retirable_idle(&self, now: MonotonicInstant) -> Option<usize> {
        if !self.policy.idle_retire() || self.counts.installed_workers <= self.policy.min_workers()
        {
            return None;
        }
        let retire_after = self.policy.idle_retire_after()?;
        self.workers.iter().position(|worker| {
            worker.state == WorkerState::Idle
                && worker.idle_since.is_some_and(|idle_since| {
                    now.saturating_duration_since(idle_since) >= retire_after
                })
        })
    }

    fn mark_idle_for_wait(&mut self, index: usize, now: MonotonicInstant) -> bool {
        let already_waiting_idle = self.workers[index].state == WorkerState::Idle
            && self.workers[index].idle_since.is_some();
        if self.workers[index].state != WorkerState::Idle {
            self.transition(index, WorkerState::Idle);
        }
        if self.workers[index].idle_since.is_none() {
            self.workers[index].idle_since = Some(now);
        }
        !already_waiting_idle
    }

    fn notify_manager_for_idle_retire_if_needed(&mut self, index: usize) -> ActionBatch {
        let mut actions = ActionBatch::new();
        if !self.policy.idle_retire() || self.counts.installed_workers <= self.policy.min_workers()
        {
            return actions;
        }
        if self.policy.idle_retire_after().is_none() {
            return actions;
        };
        if self.workers[index].idle_since.is_some() {
            self.management_pending = true;
            actions.push_immediate(ImmediateAction::WakeManager { pool: self.id });
        }
        actions
    }

    fn dispatch_worker_action(&self, worker: WorkerId) -> ActionBatch {
        let mut actions = ActionBatch::new();
        if self.policy.manager_managed() {
            actions.push_immediate(ImmediateAction::WakeWorker {
                pool: self.id,
                worker,
            });
        } else {
            actions.push_immediate(ImmediateAction::RaiseBottomHalf { pool: self.id });
        }
        actions
    }

    fn find_state(&self, state: WorkerState) -> Option<usize> {
        self.workers.iter().position(|worker| worker.state == state)
    }

    fn transition(&mut self, index: usize, new: WorkerState) {
        let old = self.workers[index].state;
        if old == new {
            return;
        }
        self.counts.leave(old);
        self.workers[index].state = new;
        self.counts.enter(new);
        if !matches!(new, WorkerState::Idle) {
            self.workers[index].idle_since = None;
        }
        self.debug_assert_invariants();
    }

    #[cfg(debug_assertions)]
    fn debug_assert_invariants(&self) {
        let mut expected = WorkerCounts::new();
        for worker in &self.workers {
            expected.enter(worker.state);
            let has_thread_ref = worker.thread_ref.is_some();
            match worker.state {
                WorkerState::Empty | WorkerState::Creating => debug_assert!(!has_thread_ref),
                WorkerState::Idle
                | WorkerState::RetireRequested
                | WorkerState::Exiting
                | WorkerState::Preparing
                | WorkerState::Claiming
                | WorkerState::Running
                | WorkerState::Sleeping => debug_assert!(has_thread_ref),
            }
        }
        expected.nr_concurrency = self.counts.nr_concurrency;
        debug_assert_eq!(self.counts, expected);
    }

    #[cfg(not(debug_assertions))]
    fn debug_assert_invariants(&self) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerCounts {
    /// Runtime worker slots that have a task or placeholder installed.
    ///
    /// `Creating` is intentionally excluded: it is only a reserved slot while
    /// the manager is trying to create the runtime worker.
    installed_workers: usize,
    nr_creating: usize,
    nr_idle: usize,
    nr_preparing: usize,
    nr_claiming: usize,
    nr_running_state: usize,
    nr_sleeping: usize,
    nr_retire_requested: usize,
    nr_exiting: usize,
    /// Running executions that still consume pool concurrency.
    ///
    /// This is not a lifecycle-state count. A `Running` execution contributes
    /// only while its accounting is `Normal`; `Sleeping` and `CpuIntensive`
    /// executions are excluded.
    nr_concurrency: usize,
}

impl WorkerCounts {
    const fn new() -> Self {
        Self {
            installed_workers: 0,
            nr_creating: 0,
            nr_idle: 0,
            nr_preparing: 0,
            nr_claiming: 0,
            nr_running_state: 0,
            nr_sleeping: 0,
            nr_retire_requested: 0,
            nr_exiting: 0,
            nr_concurrency: 0,
        }
    }

    fn enter(&mut self, state: WorkerState) {
        match state {
            WorkerState::Empty => {}
            WorkerState::Creating => self.nr_creating += 1,
            WorkerState::Idle => {
                self.installed_workers += 1;
                self.nr_idle += 1;
            }
            WorkerState::RetireRequested => {
                self.installed_workers += 1;
                self.nr_retire_requested += 1;
            }
            WorkerState::Exiting => {
                self.installed_workers += 1;
                self.nr_exiting += 1;
            }
            WorkerState::Preparing => {
                self.installed_workers += 1;
                self.nr_preparing += 1;
            }
            WorkerState::Claiming => {
                self.installed_workers += 1;
                self.nr_claiming += 1;
            }
            WorkerState::Running => {
                self.installed_workers += 1;
                self.nr_running_state += 1;
            }
            WorkerState::Sleeping => {
                self.installed_workers += 1;
                self.nr_sleeping += 1;
            }
        }
    }

    fn leave(&mut self, state: WorkerState) {
        match state {
            WorkerState::Empty => {}
            WorkerState::Creating => self.nr_creating = self.nr_creating.saturating_sub(1),
            WorkerState::Idle => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_idle = self.nr_idle.saturating_sub(1);
            }
            WorkerState::RetireRequested => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_retire_requested = self.nr_retire_requested.saturating_sub(1);
            }
            WorkerState::Exiting => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_exiting = self.nr_exiting.saturating_sub(1);
            }
            WorkerState::Preparing => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_preparing = self.nr_preparing.saturating_sub(1);
            }
            WorkerState::Claiming => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_claiming = self.nr_claiming.saturating_sub(1);
            }
            WorkerState::Running => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_running_state = self.nr_running_state.saturating_sub(1);
            }
            WorkerState::Sleeping => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_sleeping = self.nr_sleeping.saturating_sub(1);
            }
        }
    }
}
