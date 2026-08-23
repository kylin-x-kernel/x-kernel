// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

use super::{
    Worker, WorkerExecutionToken, WorkerId, WorkerSleepTransition, WorkerState, WorkerWakePlan,
};
use crate::{
    BarrierAttachResult, MAX_SYSTEM_WORKERS_PER_CPU, PendingBarrierAttach, PendingWorkEntry,
    PendingWorkStore, QueueOwner, RunQueueEntryClaim, ScheduledWork, WorkBarrier, WorkColor,
    WorkEntry, WorkInstanceId, WorkQueuePoolBinding, WorkQueuePoolState, WorkStatus,
    WorkqueueTimerIf,
};

/// Default CPU-intensive threshold for one worker pool.
///
/// A worker whose current callback runs for at least this long stops counting
/// toward the pool's `nr_running` concurrency accounting, so queued work can
/// wake another worker. This mirrors the Linux default of
/// `wq_cpu_intensive_thresh_init` (10 ms on normal hardware).
pub(crate) const DEFAULT_CPU_INTENSIVE_THRESHOLD: TimeSpan = TimeSpan::from_millis(10);

/// Default backoff after provider-side dynamic worker creation fails.
///
/// Linux uses timer-based retry/mayday paths instead of retrying worker
/// creation in a tight loop. X-Kernel keeps the same pressure-control
/// property while the task provider owns actual worker task creation.
pub const WORKER_CREATE_RETRY_DELAY: TimeSpan = TimeSpan::from_millis(10);

/// Execution domain backed by a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolExecution {
    /// Sleepable task-context workers created by the task provider.
    Task,
    /// Bottom-half drain context without provider-owned worker tasks.
    BottomHalf,
}

/// Scheduling class for workers or drain contexts attached to a pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolSchedulingPolicy {
    /// Normal-priority execution.
    Normal,
    /// High-priority execution.
    HighPriority,
}

/// CPU placement policy for a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolCpuAffinity {
    /// Workers or drain context are pinned to one logical CPU.
    Pinned(LogicalCpuId),
}

/// Execution attributes shared by all work drained from one worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolAttrs {
    execution: WorkerPoolExecution,
    scheduling: WorkerPoolSchedulingPolicy,
    cpu_affinity: WorkerPoolCpuAffinity,
}

impl WorkerPoolAttrs {
    /// Creates worker-pool execution attributes.
    pub const fn new(
        execution: WorkerPoolExecution,
        scheduling: WorkerPoolSchedulingPolicy,
        cpu_affinity: WorkerPoolCpuAffinity,
    ) -> Self {
        Self {
            execution,
            scheduling,
            cpu_affinity,
        }
    }

    /// Returns the execution domain.
    pub const fn execution(self) -> WorkerPoolExecution {
        self.execution
    }

    /// Returns the scheduling policy.
    pub const fn scheduling(self) -> WorkerPoolSchedulingPolicy {
        self.scheduling
    }

    /// Returns the CPU affinity policy.
    pub const fn cpu_affinity(self) -> WorkerPoolCpuAffinity {
        self.cpu_affinity
    }
}

pub(crate) struct WorkerPoolPendingRemoval {
    entry: Option<PendingWorkEntry>,
}

impl WorkerPoolPendingRemoval {
    pub(crate) fn entry(&self) -> Option<&WorkEntry> {
        self.entry.as_ref().map(PendingWorkEntry::entry)
    }

    pub(crate) fn entry_mut(&mut self) -> Option<&mut WorkEntry> {
        self.entry.as_mut().map(PendingWorkEntry::entry_mut)
    }

    pub(crate) fn is_runnable(&self) -> bool {
        self.entry
            .as_ref()
            .is_some_and(PendingWorkEntry::is_runnable)
    }

    pub(crate) fn into_work(self) -> Option<ScheduledWork> {
        self.entry.map(PendingWorkEntry::into_work)
    }
}

pub(crate) struct WorkerPoolRunnableTake {
    pub(crate) work: Option<ScheduledWork>,
    pub(crate) binding: Option<WorkQueuePoolBinding>,
    pub(crate) worker_token: Option<WorkerExecutionToken>,
    pub(crate) completed_barriers: alloc::vec::Vec<WorkBarrier>,
    pub(crate) stale_entries: alloc::vec::Vec<WorkerPoolStaleEntry>,
}

pub(crate) struct WorkerPoolStaleEntry {
    pub(crate) owner: QueueOwner,
    pub(crate) status: WorkStatus,
    pub(crate) color: WorkColor,
    pub(crate) barrier_count: usize,
}

impl WorkerPoolRunnableTake {
    fn none() -> Self {
        Self {
            work: None,
            binding: None,
            worker_token: None,
            completed_barriers: alloc::vec::Vec::new(),
            stale_entries: alloc::vec::Vec::new(),
        }
    }

    fn with_work(
        mut self,
        work: ScheduledWork,
        binding: WorkQueuePoolBinding,
        worker_token: WorkerExecutionToken,
    ) -> Self {
        self.work = Some(work);
        self.binding = Some(binding);
        self.worker_token = Some(worker_token);
        self
    }
}

pub(crate) struct WorkerPool {
    attrs: SpinNoIrq<Option<WorkerPoolAttrs>>,
    stats: WorkerPoolStats,
    pub(crate) state: SpinNoIrq<WorkerPoolState>,
}

impl WorkerPool {
    pub(crate) const fn new() -> Self {
        Self {
            attrs: SpinNoIrq::new(None),
            stats: WorkerPoolStats::new(),
            state: SpinNoIrq::new(WorkerPoolState::new()),
        }
    }

    pub(crate) fn ensure_attrs(&self, attrs: WorkerPoolAttrs) {
        let mut current = self.attrs.lock();
        match *current {
            Some(existing) if existing != attrs => {
                warn!("worker pool attribute mismatch: existing={existing:?}, requested={attrs:?}");
            }
            Some(_) => {}
            None => *current = Some(attrs),
        }
    }

    pub fn attrs(&self) -> Option<WorkerPoolAttrs> {
        *self.attrs.lock()
    }

    pub(crate) fn key(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    pub(crate) fn note_progress(&self) {
        self.stats.note_progress(WorkqueueTimerIf::monotonic_time());
    }

    pub(crate) fn update_runnable_stats(&self, state: &WorkerPoolState) {
        self.stats
            .update_runnable(state.runnable_count, WorkqueueTimerIf::monotonic_time());
    }

    pub(crate) fn stats_snapshot(&self) -> WorkerPoolStatsSnapshot {
        self.stats.snapshot()
    }
}

pub(crate) struct WorkerPoolStatsSnapshot {
    pub(crate) runnable_count: usize,
    pub(crate) runnable_since: Option<MonotonicInstant>,
    pub(crate) last_progress: Option<MonotonicInstant>,
}

struct WorkerPoolStats {
    runnable_count: AtomicUsize,
    runnable_since_ns: AtomicU64,
    last_progress_ns: AtomicU64,
}

impl WorkerPoolStats {
    const fn new() -> Self {
        Self {
            runnable_count: AtomicUsize::new(0),
            runnable_since_ns: AtomicU64::new(0),
            last_progress_ns: AtomicU64::new(0),
        }
    }

    fn note_progress(&self, now: MonotonicInstant) {
        self.last_progress_ns
            .store(encode_instant_ns(now), Ordering::Release);
    }

    fn update_runnable(&self, runnable_count: usize, now: MonotonicInstant) {
        self.runnable_count.store(runnable_count, Ordering::Release);
        if runnable_count == 0 {
            self.runnable_since_ns.store(0, Ordering::Release);
            return;
        }

        let _ = self.runnable_since_ns.compare_exchange(
            0,
            encode_instant_ns(now),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn snapshot(&self) -> WorkerPoolStatsSnapshot {
        WorkerPoolStatsSnapshot {
            runnable_count: self.runnable_count.load(Ordering::Acquire),
            runnable_since: decode_instant_ns(self.runnable_since_ns.load(Ordering::Acquire)),
            last_progress: decode_instant_ns(self.last_progress_ns.load(Ordering::Acquire)),
        }
    }
}

fn encode_instant_ns(now: MonotonicInstant) -> u64 {
    now.as_nanos_u64_saturating().max(1)
}

fn decode_instant_ns(nanos: u64) -> Option<MonotonicInstant> {
    (nanos != 0).then(|| MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(nanos)))
}

/// Test-only state of one pool wake freeze.
///
/// Kernel preemption lets live system workers preempt a unit test and drain
/// the shared per-CPU pool mid-test. A frozen pool gives exclusive control to
/// the test: wake plans are buffered instead of executed, workers already in
/// a wait loop re-block, and provider-side drain loops stop (see
/// `SystemPoolBinding::freeze_wakes_for_tests`).
#[cfg(unittest)]
pub(crate) struct PoolTestFreeze {
    /// Wake plans buffered while frozen.
    deferred: WorkerWakePlan,
    /// `manager_needed` as observed at freeze time, restored on a discard
    /// unfreeze so buffered kick evaluations leave no stale manager request.
    saved_manager_needed: bool,
}

pub(crate) struct WorkerPoolState {
    pub(crate) pending: PendingWorkStore,
    pub(crate) runnable_count: usize,
    pub(crate) nr_running: usize,
    pub(crate) manager_needed: bool,
    pub(crate) manager_active: bool,
    pub(crate) installed_workers: usize,
    pub(crate) workers: [Worker; MAX_SYSTEM_WORKERS_PER_CPU],
    /// How long a callback may run before its worker stops counting toward
    /// `nr_running`.
    ///
    /// This is the X-Kernel counterpart of Linux `pool->cpu_intensive_thresh`.
    cpu_intensive_threshold: TimeSpan,
    /// Earliest time a failed dynamic worker creation may be retried.
    ///
    /// This is a pool-level throttle for provider resource pressure. It does
    /// not change workqueue policy flags and does not implement MEM_RECLAIM
    /// rescuer semantics.
    worker_create_retry_after: Option<MonotonicInstant>,
    worker_create_retry_delay: TimeSpan,
    /// Active test wake freeze, if any.
    #[cfg(unittest)]
    test_freeze: Option<PoolTestFreeze>,
}

impl WorkerPoolState {
    const fn new() -> Self {
        Self {
            pending: PendingWorkStore::new(),
            runnable_count: 0,
            nr_running: 0,
            manager_needed: false,
            manager_active: false,
            installed_workers: 0,
            workers: [const { Worker::new() }; MAX_SYSTEM_WORKERS_PER_CPU],
            cpu_intensive_threshold: DEFAULT_CPU_INTENSIVE_THRESHOLD,
            worker_create_retry_after: None,
            worker_create_retry_delay: WORKER_CREATE_RETRY_DELAY,
            #[cfg(unittest)]
            test_freeze: None,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn set_cpu_intensive_threshold_for_tests(&mut self, threshold: TimeSpan) {
        self.cpu_intensive_threshold = threshold;
    }

    #[cfg(unittest)]
    pub(crate) fn set_worker_create_retry_delay_for_tests(&mut self, delay: TimeSpan) {
        self.worker_create_retry_delay = delay;
        if self.worker_create_retry_after.is_some() {
            self.worker_create_retry_after = Some(
                WorkqueueTimerIf::monotonic_time()
                    .checked_add(delay)
                    .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX)),
            );
        }
    }

    /// Test-only: starts a wake freeze for exclusive pool control.
    ///
    /// A second freeze while one is active keeps the first freeze state, so
    /// nested test helpers cannot clobber the saved `manager_needed` value.
    #[cfg(unittest)]
    pub(crate) fn freeze_wakes_for_tests(&mut self) {
        if self.test_freeze.is_some() {
            return;
        }
        self.test_freeze = Some(PoolTestFreeze {
            deferred: WorkerWakePlan::default(),
            saved_manager_needed: self.manager_needed,
        });
    }

    /// Test-only: returns whether a wake freeze is active.
    #[cfg(unittest)]
    pub(crate) fn is_wake_frozen_for_tests(&self) -> bool {
        self.test_freeze.is_some()
    }

    /// Test-only: buffers `plan` in the active freeze instead of waking.
    ///
    /// Returns `true` when the pool is frozen and the plan was deferred. At
    /// most one idle worker can be kicked while frozen (a pending `Preparing`
    /// slot keeps [`Self::need_more_worker`] false), so merging keeps the
    /// first selected worker and ORs manager requests.
    #[cfg(unittest)]
    pub(crate) fn defer_wake_plan_for_tests(&mut self, plan: WorkerWakePlan) -> bool {
        let Some(freeze) = self.test_freeze.as_mut() else {
            return false;
        };
        if freeze.deferred.worker_to_wake.is_none() {
            freeze.deferred.worker_to_wake = plan.worker_to_wake;
        }
        freeze.deferred.should_wake_manager |= plan.should_wake_manager;
        true
    }

    /// Test-only: ends the wake freeze and returns the wake plan to execute.
    ///
    /// With `discard_deferred`, buffered wakes are dropped: `Preparing` slots
    /// flipped by buffered kick evaluations return to `Idle` and the saved
    /// `manager_needed` value is restored, leaving the pool quiescent for
    /// tests that drain work manually. Otherwise the merged deferred plan is
    /// returned so the caller can wake the pool and let live workers drain
    /// the work queued during the freeze.
    #[cfg(unittest)]
    pub(crate) fn unfreeze_wakes_for_tests(&mut self, discard_deferred: bool) -> WorkerWakePlan {
        let Some(freeze) = self.test_freeze.take() else {
            return WorkerWakePlan::default();
        };
        if !discard_deferred {
            return freeze.deferred;
        }
        self.manager_needed = freeze.saved_manager_needed;
        for worker in self.workers.iter_mut() {
            if worker.state == WorkerState::Preparing {
                worker.state = WorkerState::Idle;
                let _ = worker.clear_current_work();
            }
        }
        WorkerWakePlan::default()
    }

    pub(crate) fn has_runnable_work(&self) -> bool {
        self.runnable_count != 0
    }

    #[cfg(unittest)]
    pub(crate) fn pending_len_for_binding(&self, binding_key: usize) -> usize {
        self.pending.pending_len_for_binding(binding_key)
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_binding(&self, binding_key: usize) -> usize {
        self.pending.runnable_len_for_binding(binding_key)
    }

    pub(crate) fn configure_binding_max_active(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
        max_active: usize,
    ) {
        binding.configure_max_active(max_active);
        let mut runnable_budget = binding.reset_active_to_running();
        let deactivated = self.pending.deactivate_runnable_for_binding(binding_key);
        self.runnable_count = self.runnable_count.saturating_sub(deactivated);

        while runnable_budget != 0
            && self
                .pending
                .activate_first_inactive_for_binding(binding_key)
        {
            binding.add_active();
            self.runnable_count += 1;
            runnable_budget -= 1;
        }
    }

    pub(crate) fn need_more_worker(&self) -> bool {
        self.runnable_count != 0
            && self.nr_running == 0
            && !self
                .workers
                .iter()
                .any(|worker| worker.state == WorkerState::Preparing)
    }

    pub(crate) fn worker_creation_retry_ready(&self) -> bool {
        self.worker_create_retry_after
            .is_none_or(|deadline| WorkqueueTimerIf::monotonic_time() >= deadline)
    }

    pub(crate) fn manager_should_run(&self) -> bool {
        self.manager_needed && !self.manager_active && self.worker_creation_retry_ready()
    }

    pub(crate) fn installed_worker_count(&self) -> usize {
        self.installed_workers
    }

    fn first_idle_worker(&self) -> Option<usize> {
        self.workers
            .iter()
            .position(|worker| worker.state == WorkerState::Idle)
    }

    pub(crate) fn select_worker_to_kick(&mut self) -> WorkerWakePlan {
        self.auto_mark_cpu_intensive();
        if !self.need_more_worker() {
            return WorkerWakePlan {
                worker_to_wake: None,
                should_wake_manager: false,
            };
        }
        if let Some(worker_id) = self.first_idle_worker() {
            self.workers[worker_id].state = WorkerState::Preparing;
            return WorkerWakePlan {
                worker_to_wake: Some(WorkerId::new(worker_id)),
                should_wake_manager: false,
            };
        }

        let should_wake_manager = !self.manager_active && self.worker_creation_retry_ready();
        if !self.manager_active {
            self.manager_needed = true;
        }
        WorkerWakePlan {
            worker_to_wake: None,
            should_wake_manager,
        }
    }

    /// Marks running workers that exceeded the CPU-intensive threshold.
    ///
    /// Demand-driven fallback for Linux `wq_worker_tick` marking.
    ///
    /// The scheduler tick marks long-running workers while they execute. This
    /// fallback keeps enqueue/block/finish kick evaluation sufficient when a
    /// runnable work arrives after a worker has already crossed the threshold.
    /// Marked workers leave `nr_running`, which lets [`Self::need_more_worker`]
    /// wake another worker for queued work. The timestamp comes from the timer
    /// provider, which is a lock-free counter read and therefore safe under the
    /// pool lock in IRQ-safe enqueue paths.
    fn auto_mark_cpu_intensive(&mut self) {
        if self.runnable_count == 0 || self.nr_running == 0 {
            return;
        }
        let now = WorkqueueTimerIf::monotonic_time();
        for worker in self.workers.iter_mut() {
            if worker.state != WorkerState::Running || worker.cpu_intensive {
                continue;
            }
            if worker.run_exceeds(self.cpu_intensive_threshold, now) {
                worker.cpu_intensive = true;
                self.nr_running = self.nr_running.saturating_sub(1);
            }
        }
    }

    pub(crate) fn remove_pending_work(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
    ) -> WorkerPoolPendingRemoval {
        let removed = self.pending.remove_work_for_key(work, binding_key);
        if removed.as_ref().is_some_and(PendingWorkEntry::is_runnable) {
            self.runnable_count = self.runnable_count.saturating_sub(1);
        }
        WorkerPoolPendingRemoval { entry: removed }
    }

    pub(crate) fn remove_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        removed: &WorkerPoolPendingRemoval,
    ) -> bool {
        if !removed.is_runnable() {
            return false;
        }

        let Some(entry) = removed.entry() else {
            return false;
        };
        self.discard_active_entry_locked(binding, entry.binding_key())
    }

    pub(crate) fn discard_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        binding.remove_active();
        self.activate_next_inactive_locked(binding, binding_key)
    }

    pub(crate) fn finish_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        binding.finish_active_work();
        self.activate_next_inactive_locked(binding, binding_key)
    }

    fn activate_next_inactive_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        if !binding.can_activate() {
            return false;
        }

        if self
            .pending
            .activate_first_inactive_for_binding(binding_key)
        {
            binding.add_active();
            self.runnable_count += 1;
            return true;
        }
        false
    }

    pub(crate) fn attach_pending_barrier(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        self.pending
            .attach_barrier_to_work_for_key(work, binding_key, barrier)
    }

    pub(crate) fn take_any_runnable_work(
        &mut self,
        pool_key: usize,
        worker_id: WorkerId,
    ) -> WorkerPoolRunnableTake {
        let worker_index = worker_id.as_usize();
        let Some(slot) = self.workers.get_mut(worker_index) else {
            return WorkerPoolRunnableTake::none();
        };
        if !matches!(slot.state, WorkerState::Idle | WorkerState::Preparing) {
            return WorkerPoolRunnableTake::none();
        }
        if self.runnable_count == 0 {
            self.clear_worker_current_work(worker_index);
            return WorkerPoolRunnableTake::none();
        }

        let mut outcome = WorkerPoolRunnableTake::none();
        while let Some(mut entry) = self.pending.pop_runnable_candidate() {
            let binding_key = entry.binding_key();
            let instance_id = entry.instance_id();
            let entry_owner = entry.owner().clone();
            let work = entry.work().clone();
            let worker_token = slot.next_execution_token();

            let decision = work.claim_pool_entry_for_run(
                pool_key,
                binding_key,
                instance_id,
                worker_id,
                worker_token,
            );
            match decision {
                RunQueueEntryClaim::Run {
                    binding,
                    instance_id,
                } => {
                    let linked_barriers = entry.take_barriers();
                    self.runnable_count = self.runnable_count.saturating_sub(1);
                    self.start_running_work_with_token(
                        worker_index,
                        work.key(),
                        instance_id,
                        worker_token,
                        linked_barriers,
                    );
                    return outcome.with_work(work, binding, worker_token);
                }
                RunQueueEntryClaim::Stale(status) => {
                    let color = entry.color();
                    let barrier_count = entry.barrier_count();
                    self.runnable_count = self.runnable_count.saturating_sub(1);
                    outcome
                        .completed_barriers
                        .append(&mut entry.take_barriers());
                    outcome.stale_entries.push(WorkerPoolStaleEntry {
                        owner: entry_owner,
                        status,
                        color,
                        barrier_count,
                    });
                }
            }
        }

        self.clear_worker_current_work(worker_index);
        outcome
    }

    pub(crate) fn install_worker(&mut self, worker_id: usize) -> bool {
        let Some(slot) = self.workers.get_mut(worker_id) else {
            return false;
        };
        if !matches!(slot.state, WorkerState::Empty | WorkerState::Creating) {
            return false;
        }
        slot.state = WorkerState::Idle;
        self.installed_workers += 1;
        true
    }

    #[cfg(unittest)]
    pub(crate) fn start_running_work(
        &mut self,
        worker_id: usize,
        work_key: usize,
        instance_id: WorkInstanceId,
        barriers: alloc::vec::Vec<WorkBarrier>,
    ) -> WorkerExecutionToken {
        let worker_token = self.workers[worker_id].next_execution_token();
        self.start_running_work_with_token(
            worker_id,
            work_key,
            instance_id,
            worker_token,
            barriers,
        );
        worker_token
    }

    fn start_running_work_with_token(
        &mut self,
        worker_id: usize,
        work_key: usize,
        instance_id: WorkInstanceId,
        worker_token: WorkerExecutionToken,
        barriers: alloc::vec::Vec<WorkBarrier>,
    ) {
        let slot = &mut self.workers[worker_id];
        debug_assert_eq!(slot.generation, worker_token);
        if slot.state != WorkerState::Running {
            self.nr_running += 1;
        }
        slot.state = WorkerState::Running;
        // The CPU-intensive flag is per-execution and was cleared when the
        // previous work finished, so the fresh increment above stays balanced.
        slot.set_current_work(
            work_key,
            instance_id,
            barriers,
            WorkqueueTimerIf::monotonic_time(),
        );
        self.manager_needed = false;
    }

    pub(crate) fn finish_running_work(
        &mut self,
        worker_id: usize,
        work_key: usize,
        instance_id: WorkInstanceId,
        worker_token: WorkerExecutionToken,
    ) -> alloc::vec::Vec<WorkBarrier> {
        let Some(slot) = self.workers.get_mut(worker_id) else {
            return alloc::vec::Vec::new();
        };
        if !slot.is_current_execution(work_key, instance_id, worker_token) {
            warn!("workerqueue worker {worker_id} finished an unexpected work instance");
            return alloc::vec::Vec::new();
        }
        match slot.state {
            // CPU-intensive workers already left `nr_running` when marked.
            WorkerState::Running if !slot.cpu_intensive => {
                self.nr_running = self.nr_running.saturating_sub(1);
            }
            WorkerState::Running | WorkerState::Sleeping => {}
            WorkerState::Creating
            | WorkerState::Preparing
            | WorkerState::Idle
            | WorkerState::Empty => {}
        }
        slot.state = WorkerState::Idle;
        // Also clears the CPU-intensive flag: marking is per-execution.
        slot.clear_current_work()
    }

    pub(crate) fn tick_running_worker(
        &mut self,
        worker_id: usize,
        worker_token: WorkerExecutionToken,
    ) -> WorkerWakePlan {
        let Some(slot) = self.workers.get_mut(worker_id) else {
            return WorkerWakePlan::default();
        };
        if slot.state != WorkerState::Running
            || slot.generation != worker_token
            || slot.cpu_intensive
            || !slot.run_exceeds(
                self.cpu_intensive_threshold,
                WorkqueueTimerIf::monotonic_time(),
            )
        {
            return WorkerWakePlan::default();
        }

        slot.cpu_intensive = true;
        self.nr_running = self.nr_running.saturating_sub(1);
        self.select_worker_to_kick()
    }

    fn clear_worker_current_work(&mut self, worker_id: usize) {
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.state = WorkerState::Idle;
            let _ = worker.clear_current_work();
        }
    }

    pub(crate) fn attach_running_barrier(
        &mut self,
        worker_id: WorkerId,
        work_key: usize,
        instance_id: WorkInstanceId,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        let Some(slot) = self.workers.get_mut(worker_id.as_usize()) else {
            return PendingBarrierAttach::Missing;
        };
        if !slot.is_running_instance(work_key, instance_id) {
            return PendingBarrierAttach::Missing;
        }
        match slot.push_running_barrier(barrier) {
            BarrierAttachResult::Attached => PendingBarrierAttach::Attached,
            BarrierAttachResult::Full => PendingBarrierAttach::Full,
        }
    }

    pub(crate) fn mark_worker_sleeping(&mut self, worker_id: usize) -> WorkerSleepTransition {
        let mut did_sleep = false;
        if let Some(slot) = self.workers.get_mut(worker_id)
            && slot.state == WorkerState::Running
        {
            slot.state = WorkerState::Sleeping;
            // Marked workers already left `nr_running`; blocking does not
            // change their accounting.
            if !slot.cpu_intensive {
                self.nr_running = self.nr_running.saturating_sub(1);
            }
            did_sleep = true;
        }
        let wake_plan = if did_sleep {
            self.select_worker_to_kick()
        } else {
            WorkerWakePlan::default()
        };
        WorkerSleepTransition {
            did_sleep,
            wake_plan,
        }
    }

    pub(crate) fn mark_worker_running(&mut self, worker_id: usize) {
        let Some(slot) = self.workers.get_mut(worker_id) else {
            return;
        };
        if slot.state == WorkerState::Sleeping {
            slot.state = WorkerState::Running;
            // Symmetric with `mark_worker_sleeping`: marked workers stay out of
            // `nr_running` across sleep/resume.
            if !slot.cpu_intensive {
                self.nr_running += 1;
            }
            self.manager_needed = false;
        }
    }

    pub(crate) fn prepare_worker_to_wait(&mut self, worker_id: usize) -> bool {
        // A frozen pool must not hand runnable work to live workers; make
        // awake workers block in their wait loop until the test unfreezes.
        #[cfg(unittest)]
        if self.test_freeze.is_some() {
            return false;
        }
        if self.runnable_count == 0
            && let Some(slot) = self.workers.get_mut(worker_id)
            && slot.state == WorkerState::Preparing
        {
            slot.state = WorkerState::Idle;
            let _ = slot.clear_current_work();
        }
        self.has_runnable_work()
    }

    pub(crate) fn reserve_worker_creation(&mut self) -> Option<usize> {
        if self.manager_active {
            return None;
        }
        if !self.worker_creation_retry_ready() {
            return None;
        }
        if !self.need_more_worker() {
            self.manager_needed = false;
            return None;
        }
        let Some(worker_id) = self
            .workers
            .iter()
            .position(|worker| worker.state == WorkerState::Empty)
        else {
            self.manager_needed = false;
            self.manager_active = false;
            return None;
        };
        self.manager_needed = false;
        self.manager_active = true;
        self.workers[worker_id].state = WorkerState::Creating;
        Some(worker_id)
    }

    pub(crate) fn finish_worker_creation(
        &mut self,
        worker_id: usize,
        success: bool,
    ) -> WorkerWakePlan {
        if let Some(slot) = self.workers.get_mut(worker_id)
            && slot.state == WorkerState::Creating
        {
            if success {
                self.worker_create_retry_after = None;
            } else {
                slot.state = WorkerState::Empty;
                let _ = slot.clear_current_work();
                self.worker_create_retry_after = Some(
                    WorkqueueTimerIf::monotonic_time()
                        .checked_add(self.worker_create_retry_delay)
                        .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX)),
                );
            }
        }
        self.manager_active = false;

        if !success && self.need_more_worker() {
            self.manager_needed = true;
            return WorkerWakePlan::default();
        }
        self.select_worker_to_kick()
    }
}
