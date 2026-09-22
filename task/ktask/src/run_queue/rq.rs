// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Leave, pick, switch, resched kick, and run-queue operations.

use alloc::sync::Arc;
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU64, Ordering};

use kcpu_id_map::LogicalCpuId;
use khal::percpu::this_cpu_id;
use ksched::{BaseScheduler, CurrentDisposition};
use kspin::{BaseGuard, SpinNoIrqGuard, SpinRaw};

#[cfg(feature = "smp")]
use super::migrate::spawn_affinity_migration_task;
#[cfg(all(feature = "preempt", feature = "smp"))]
use super::stats::record_remote_resched;
#[cfg(feature = "sched_stat")]
use super::stats::{sched_stat_cpu, sched_stat_inc};
use super::{
    CurrentRunQueueRef, KRunQueueRef, RunQueue, WakeEnqueue, current_idle_task,
    current_wait_for_exit, gc, with_exited_tasks,
};
#[cfg(feature = "smp")]
use super::{PREV_TASK, get_run_queue};
use crate::{
    KtaskRef, Scheduler, TaskInner,
    api::{
        account_execution_blocked, account_execution_context, account_execution_resumed, current,
        program_sched_deadline, program_sched_deadline_for, rearm_local_timer,
        refresh_execution_tick_deadline_for_task,
    },
    current_may_uninit,
    task::{CurrentTask, TaskState},
    tracing_hooks::{fire_context_switch, fire_task_wakeup},
};

#[cfg(feature = "preempt")]
pub(super) fn request_resched_on(rq: &RunQueue) {
    let cpu_id = rq.cpu_id;
    if cpu_id == this_cpu_id() {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(cpu_id).local_resched);
        current().set_preempt_pending(true);
        // A newly runnable peer may require an earlier schedule deadline even
        // when the current task previously needed no timer (lone task).
        let now_ns = khal::time::monotonic_time_nanos();
        let curr = current();
        let next_rel = if curr.is_idle() {
            None
        } else {
            let curr_arc = curr.clone();
            rq.scheduler.lock().next_preemption_ns(&curr_arc)
        };
        program_sched_deadline(now_ns, next_rel);
        rearm_local_timer();
    } else {
        #[cfg(all(feature = "smp", feature = "ipi"))]
        {
            let resched_result = request_remote_resched(cpu_id);
            record_remote_resched(cpu_id, resched_result.is_err());
        }
    }
}

#[cfg(all(feature = "preempt", feature = "smp", feature = "ipi"))]
pub(super) fn request_remote_resched(cpu_id: LogicalCpuId) -> kipi::Result<()> {
    if let Err(err) = kipi::notify_reschedule(cpu_id) {
        warn!(
            "failed to request reschedule on CPU {}: {}",
            cpu_id.as_usize(),
            err
        );
        Err(err)
    } else {
        Ok(())
    }
}

/// Kicks a running task off user mode or into a preempt check.
///
/// Blocked waiters are handled by [`TaskInner::interrupt`]'s `PollSet` wake.
/// A task that is `on_cpu` has no waiter, so NOHZ can leave it in user mode
/// until the next syscall or timer IRQ. Signal delivery therefore needs a CPU
/// kick: local `need_resched`, or a reschedule IPI.
///
/// Matches Linux `kick_process`: sample `on_cpu` / `cpu_id` once. A userspace
/// runner cannot change CPUs without a kernel entry that already observes
/// `interrupted`; a stale IPI is a spurious resched. The IPI itself pulls
/// `UserContext::run` back; `preempt` only adds `need_resched` in the callback.
pub(crate) fn kick_running_task(task: &TaskInner) {
    #[cfg(not(any(feature = "smp", feature = "preempt")))]
    let _ = task;
    #[cfg(feature = "smp")]
    {
        if !task.on_cpu() {
            return;
        }
        let cpu_id = task.cpu_id();
        if cpu_id == this_cpu_id() {
            #[cfg(feature = "preempt")]
            task.set_preempt_pending(true);
            return;
        }
        let kick_result = kipi::notify_reschedule(cpu_id);
        if let Err(err) = &kick_result {
            warn!(
                "failed to kick CPU {} for running task: {}",
                cpu_id.as_usize(),
                err
            );
        }
        #[cfg(feature = "preempt")]
        record_remote_resched(cpu_id, kick_result.is_err());
    }
    #[cfg(all(not(feature = "smp"), feature = "preempt"))]
    {
        if task.is_running() {
            task.set_preempt_pending(true);
        }
    }
}

impl<G: BaseGuard> Drop for KRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

impl<G: BaseGuard> Drop for CurrentRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// Management operations for run queue, including adding tasks, unblocking tasks, etc.
impl<G: BaseGuard> KRunQueueRef<'_, G> {
    /// Adds a task to the scheduler.
    ///
    /// This function is used to add a new task to the scheduler.
    pub fn add_task(&mut self, task: KtaskRef) {
        debug!(
            "task add: {} on run_queue {}",
            task.id_name(),
            self.inner.cpu_id.as_usize()
        );
        assert!(task.is_ready());
        #[cfg(feature = "snapshot")]
        {
            let _g = kspin::NoPreempt::new();
            crate::task_registry::record_tracked_task(&task);
        }
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.inner.cpu_id).add_task);
        // Flush wall time into the running entity before placement so a NOHZ
        // lone task cannot leave system V stale across add/wake.
        let now_ns = khal::time::monotonic_time_nanos();
        self.inner.flush_running_runtime(now_ns);
        self.inner.publish_task(task);

        // Dynamic schedule timers are disarmed while a task runs alone, so
        // every newly published peer must refresh the target CPU. This is also
        // required for a busy local CPU: there is no periodic tick to discover
        // the new ready entity later.
        #[cfg(feature = "preempt")]
        request_resched_on(self.inner);
    }

    /// Unblock one task by inserting it into the run queue.
    ///
    /// This function does nothing if the task is not in [`TaskState::Blocked`],
    /// which means the task is already unblocked by other cores.
    pub fn unblock_task(&mut self, task: KtaskRef, resched: bool) {
        let task_id_name = task.id_name();
        let task_id = task.trace_id();
        // Try to change the state of the task from `Blocked` to `Ready`,
        // if successful, the task will be put into this run queue,
        // otherwise, the task is already unblocked by other cores.
        // Note:
        // target task can not be insert into the run queue until it finishes its scheduling process.
        // Account + PLACE + WF_SYNC share one scheduler lock so a remote CPU
        // cannot leave/pick between flush and nominate (HEAD had no flush).
        let is_wake_sync = current_may_uninit().is_some_and(|c| c.is_wake_sync());
        let wake_enqueue =
            self.inner
                .put_task_with_state(task, TaskState::Blocked, is_wake_sync, resched);
        if wake_enqueue != WakeEnqueue::Rejected {
            // Since now, the task to be unblocked is in the `Ready` state.
            let cpu_id = self.inner.cpu_id;
            debug!(
                "task unblock: {task_id_name} on run_queue {}",
                cpu_id.as_usize()
            );
            #[cfg(feature = "sched_stat")]
            sched_stat_inc(&sched_stat_cpu(cpu_id).unblock_task);

            // Fire the task wakeup tracepoint.
            fire_task_wakeup(task_id);

            #[cfg(feature = "preempt")]
            if resched && wake_enqueue == WakeEnqueue::Enqueued {
                request_resched_on(self.inner);
            }
        }
    }
}

/// Core functions of run queue.
impl<G: BaseGuard> CurrentRunQueueRef<'_, G> {
    /// Accounts wall-clock runtime since the last account point for `current`.
    ///
    /// Matches Linux `update_curr`: request-done may set `need_resched`. Peer
    /// pick comparison belongs on the schedule-tick account path.
    pub fn account_current_runtime(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, false);
    }

    /// Linux `entity_tick`: account, then EEVDF `check_preempt_tick`.
    ///
    /// Only the due schedule slot uses this. Soft/periodic IRQs must not
    /// re-evaluate pick — that ping-ponged WF_SYNC later-deadline handoffs.
    #[cfg(feature = "sched_eevdf")]
    pub fn account_sched_tick(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, true);
    }

    fn account_runtime(&mut self, now_ns: u64, is_sched_tick: bool) {
        let curr = &self.current_task;
        let is_idle = curr.is_idle();
        let (elapsed, should_preempt) = {
            let mut sched = self.inner.scheduler.lock();
            let elapsed = RunQueue::advance_account_epoch(&self.inner.last_accounted_ns, now_ns);
            if is_idle {
                (elapsed, false)
            } else {
                let curr_arc = curr.clone();
                let mut should_preempt = elapsed > 0 && sched.update_current(&curr_arc, elapsed);
                #[cfg(feature = "sched_eevdf")]
                if is_sched_tick && !should_preempt {
                    should_preempt = sched.check_preempt_tick();
                }
                #[cfg(not(feature = "sched_eevdf"))]
                let _ = is_sched_tick;
                (elapsed, should_preempt)
            }
        };
        self.apply_runtime_account_side_effects(elapsed, is_idle, should_preempt);
    }

    fn apply_runtime_account_side_effects(
        &mut self,
        elapsed: u64,
        is_idle: bool,
        should_preempt: bool,
    ) {
        if is_idle {
            return;
        }
        #[cfg(feature = "sched_stat")]
        if should_preempt && elapsed > 0 {
            sched_stat_inc(&sched_stat_cpu(self.inner.cpu_id).tick_preempt);
        }
        #[cfg(not(feature = "sched_stat"))]
        let _ = elapsed;
        account_execution_context(self.current_task.execution_context());
        if should_preempt {
            #[cfg(feature = "preempt")]
            self.current_task.set_preempt_pending(true);
        }
    }

    /// Recomputes the per-CPU schedule deadline for the running task.
    pub fn refresh_sched_deadline(&mut self, now_ns: u64) {
        let curr = &self.current_task;
        let next_rel = if curr.is_idle() {
            None
        } else {
            let curr_arc = curr.clone();
            self.inner.scheduler.lock().next_preemption_ns(&curr_arc)
        };
        program_sched_deadline(now_ns, next_rel);
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub fn yield_current(&mut self) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        trace!("task yield: {}", curr.id_name());
        assert!(curr.is_running());

        // Affinity may have been tightened by another thread; leave this CPU
        // instead of requeueing onto a forbidden run queue.
        #[cfg(feature = "smp")]
        if !curr.cpumask().get(this_cpu_id().as_usize()) {
            let migration_task = spawn_affinity_migration_task(curr.clone());
            self.migrate_current(migration_task);
            return;
        }

        assert!(
            curr.transition_state(TaskState::Running, TaskState::Ready),
            "yield_current requires Running -> Ready before leave_current"
        );
        // Idle is never tracked by the algorithm; only flip Ready so `resched`
        // can re-select it (or another task) without enqueueing into the RQ.
        // EEVDF `curr` stays unset for idle (see `switch_to_local` sync).
        if !curr.is_idle() {
            self.inner
                .scheduler
                .lock()
                .leave_current(curr.clone(), CurrentDisposition::Yield);
        }
        self.inner.resched();
    }

    /// Migrate the current task to a new run queue matching its CPU affinity and reschedule.
    /// This function will spawn a new `migration_task` to perform the migration, which will set
    /// current task to `Ready` state and select a proper run queue for it according to its CPU affinity,
    /// switch to the migration task immediately after migration task is prepared.
    ///
    /// Note: the ownership if migrating task (which is current task) is handed over to the migration task,
    /// before the migration task inserted it into the target run queue.
    #[cfg(feature = "smp")]
    pub fn migrate_current(&mut self, migration_task: KtaskRef) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        trace!("task migrate: {}", curr.id_name());
        assert!(curr.is_running());

        // Migration helper already owns a strong ref (spawned with curr.clone()).
        // Deactivate on the source RQ before departure; destination `enqueue_task`
        // then applies PLACE_LAG via `needs_place`.
        self.inner
            .scheduler
            .lock()
            .leave_current(curr.clone(), CurrentDisposition::Migrate);
        #[cfg(feature = "smp")]
        self.inner.discharge_rq_load(curr);

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Migration helper is switched to without enqueue; attach ownership first.
        self.inner.switch_to_local(current(), migration_task);
    }

    /// Preempts the current task and reschedules.
    /// This function is used to preempt the current task and reschedule
    /// to next task on current run queue.
    ///
    /// This function is called by `current_check_preempt_pending` with IRQs and preemption disabled.
    ///
    /// Note:
    /// preemption may happened in `enable_preempt`, which is called
    /// each time a [`kspin::NoPreemptGuard`] is dropped.
    #[cfg(feature = "preempt")]
    pub fn preempt_resched(&mut self) {
        // There is no need to disable IRQ and preemption here, because
        // they both have been disabled in `current_check_preempt_pending`.
        let now_ns = khal::time::monotonic_time_nanos();
        assert!(self.current_task.is_running());

        // When we call `preempt_resched()`, both IRQs and preemption must
        // have been disabled by `kspin::NoPreemptIrqSave`. So we need
        // to set `current_disable_count` to 1 in `can_preempt()` to obtain
        // the preemption permission.
        let can_preempt = self.current_task.can_preempt(1);
        #[cfg(feature = "sched_stat")]
        {
            let stats = sched_stat_cpu(self.inner.cpu_id);
            sched_stat_inc(&stats.preempt_resched);
            if !can_preempt {
                sched_stat_inc(&stats.preempt_denied);
            }
        }

        trace!(
            "current task is to be preempted: {}, allow={}",
            self.current_task.id_name(),
            can_preempt
        );

        #[cfg(feature = "sched_eevdf")]
        let affinity_ok = {
            #[cfg(feature = "smp")]
            {
                self.current_task.cpumask().get(this_cpu_id().as_usize())
            }
            #[cfg(not(feature = "smp"))]
            {
                true
            }
        };

        #[cfg(feature = "sched_eevdf")]
        if can_preempt && affinity_ok {
            // One scheduler lock: account, EEVDF peer probe, and — when the
            // probe fails — next_preemption_ns. Re-arm hardware after release.
            let curr_arc = self.current_task.clone();
            let is_idle = self.current_task.is_idle();
            let (elapsed, should_preempt, peer_wins, next_rel) = self
                .inner
                .account_peer_probe_and_next_deadline(now_ns, &curr_arc, is_idle);
            self.apply_runtime_account_side_effects(elapsed, is_idle, should_preempt);
            if !peer_wins {
                self.current_task.set_preempt_pending(false);
                program_sched_deadline(now_ns, next_rel);
                rearm_local_timer();
                return;
            }
        } else {
            self.account_current_runtime(now_ns);
        }

        #[cfg(not(feature = "sched_eevdf"))]
        self.account_current_runtime(now_ns);

        let curr = &self.current_task;
        if can_preempt {
            // Affinity violations always migrate, even when no ready peer would
            // win an ordinary EEVDF probe (Linux `__set_cpus_allowed_ptr`).
            #[cfg(feature = "smp")]
            if !curr.cpumask().get(this_cpu_id().as_usize()) {
                let migration_task = spawn_affinity_migration_task(curr.clone());
                self.migrate_current(migration_task);
                return;
            }
            assert!(
                curr.transition_state(TaskState::Running, TaskState::Ready),
                "preempt_resched requires Running -> Ready before leave_current"
            );
            if !curr.is_idle() {
                // The into_raw current slot already counts as one strong ref.
                // leave_current(Preempt) requeues a second Arc so the task stays
                // alive across switch_to's current-pointer handoff.
                self.inner
                    .scheduler
                    .lock()
                    .leave_current(curr.clone(), CurrentDisposition::Preempt);
            }
            self.inner.resched();
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub fn exit_current(&mut self, exit_code: i32) -> ! {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        debug!("task exit: {}, exit_code={}", curr.id_name(), exit_code);
        assert!(curr.is_running(), "task is not running: {:?}", curr.state());
        assert!(!curr.is_idle());
        if curr.is_init() {
            // SAFETY: `exit_current` runs under
            // `current_run_queue::<NoPreemptIrqSave>()`, so IRQs and
            // preemption are disabled while touching the current CPU's percpu
            // exited-task list.
            with_exited_tasks(|tasks| tasks.clear());
            khal::power::power_off();
        } else {
            // Notify the joiner task.
            curr.notify_exit(exit_code);

            // SAFETY: `exit_current` runs under
            // `current_run_queue::<NoPreemptIrqSave>()`, so IRQs and
            // preemption are disabled while touching current-CPU percpu
            // scheduler queues and wakers.
            // Push current task to the `EXITED_TASKS` list, which will be consumed by the GC task.
            with_exited_tasks(|tasks| tasks.push_back(curr.clone()));
            // Wake up the GC task to drop the exited tasks.
            current_wait_for_exit().wake();

            // Exited list owns the task; `leave_current(Exit)` clears scheduler
            // current accounting (e.g. EEVDF `curr`) and must not arm PLACE_LAG.
            self.inner
                .scheduler
                .lock()
                .leave_current(curr.clone(), CurrentDisposition::Exit);
            #[cfg(feature = "smp")]
            self.inner.discharge_rq_load(curr);
            #[cfg(feature = "smp")]
            self.inner.discharge_home(curr);

            // Schedule to next task.
            self.inner.resched();
        }
        unreachable!("task exited!");
    }

    /// Block the current task and reschedule.
    ///
    /// Marks the current task `Blocked` and leaves the scheduler without requeue
    /// ([`CurrentDisposition::Block`]).
    ///
    /// Workerqueue callbacks are accounted here, not in higher-level wait
    /// wrappers: this is the scheduler's Running→Blocked convergence point and
    /// therefore the X-Kernel counterpart of Linux `wq_worker_sleeping()`.
    /// Callers must not add a second workerqueue sleep/resume wrapper around
    /// this function.
    ///
    /// # Caller obligations
    ///
    /// 1. The caller must hold the wait-queue / waker lock represented by `woke`.
    /// 2. The current task must be running and must not be idle.
    /// 3. The caller must keep an **extra** [`KtaskRef`] to the current task across
    ///    the context switch. The into_raw current slot contributes one strong
    ///    ref; the caller's clone (e.g. from [`crate::future::block_on`]) is the
    ///    second. Together they satisfy `switch_to`'s `strong_count > 1` check.
    ///    `leave_current(Block)` does not requeue, so that caller-owned clone is
    ///    what keeps the task alive until the wait queue / waker takes over.
    ///    Do not drop the clone thinking the current slot "does not count".
    /// 4. The wait-queue lock is released inside this function before rescheduling.
    ///
    /// # Panics
    ///
    /// Panics if `Arc::strong_count(current) <= 1` (missing caller-owned ref).
    /// This is a hard scheduler invariant: there is no recovery path. New
    /// wait-queue / timer callers must clone current before entry — see the
    /// `blocked_resched_survives_with_caller_owned_ref` unittest for a
    /// `block_on`-free template. `#[track_caller]` points the panic at the
    /// call site.
    #[track_caller]
    pub fn blocked_resched(&mut self, mut woke: SpinNoIrqGuard<'_, bool>) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        assert!(curr.is_running());
        assert!(!curr.is_idle());
        // we must not block current task with preemption disabled.
        // Current expected preempt count is 2 for `NoPreemptIrqSave` because we also hold
        // the `woke` SpinNoIrqGuard lock here.
        #[cfg(feature = "preempt")]
        assert!(curr.can_preempt(2));
        // Current slot (into_raw) = 1; caller's clone (e.g. block_on) = 2nd.
        let strong = Arc::strong_count(curr);
        assert!(
            strong > 1,
            "blocked_resched: strong_count={strong} (need > 1); into_raw current slot counts as 1 \
             — caller must hold another KtaskRef across switch (see block_on / blocked_resched \
             rustdoc)"
        );

        // Deactivate while still Running / current, before any concurrent wake
        // can requeue us. Fair schedulers snapshot lag for later enqueue PLACE_LAG.
        account_execution_blocked(curr.execution_context());
        self.inner
            .scheduler
            .lock()
            .leave_current(curr.clone(), CurrentDisposition::Block);
        #[cfg(feature = "smp")]
        self.inner.discharge_rq_load(curr);

        // Mark the task as blocked, this has to be done before adding it to the wait queue
        // while holding the lock of the wait queue.
        curr.set_state(TaskState::Blocked);
        *woke = false;
        drop(woke);

        // Current task's state has been changed to `Blocked` and added to the wait queue.
        // Note that the state may have been set as `Ready` in `unblock_task()`,
        // see `unblock_task()` for details.

        debug!("task block: {}", curr.id_name());
        self.inner.resched();
    }

    pub fn set_current_priority(&mut self, prio: isize) -> bool {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let ok = self
            .inner
            .scheduler
            .lock()
            .set_priority(&self.current_task, prio);
        if ok {
            self.refresh_sched_deadline(now_ns);
            rearm_local_timer();
        }
        ok
    }
}

impl<G: BaseGuard> KRunQueueRef<'_, G> {
    pub fn set_task_priority(&mut self, task: &KtaskRef, prio: isize) -> bool {
        let now_ns = khal::time::monotonic_time_nanos();
        self.inner.flush_running_runtime(now_ns);
        let ok = self.inner.scheduler.lock().set_priority(task, prio);
        #[cfg(feature = "preempt")]
        if ok {
            // Weight/deadline changes must re-arbitrate the target CPU timer;
            // otherwise NOHZ keeps waiting on a stale schedule deadline.
            request_resched_on(self.inner);
        }
        ok
    }
}

impl RunQueue {
    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    pub(super) fn new(cpu_id: LogicalCpuId) -> Self {
        let gc_task = gc::spawn_gc_task(cpu_id);

        let scheduler = Scheduler::new();
        #[cfg(all(feature = "sched_stat", feature = "sched_eevdf"))]
        let scheduler = {
            let mut scheduler = scheduler;
            scheduler.set_stats_enabled(true);
            scheduler
        };
        let rq = Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
            last_accounted_ns: AtomicU64::new(0),
            #[cfg(feature = "smp")]
            nr_running: AtomicUsize::new(0),
            #[cfg(feature = "smp")]
            nr_home: AtomicUsize::new(0),
        };
        // Publish through the ownership-aware entry so secondary-CPU gc tasks
        // do not keep the default `cpu_id == 0`.
        rq.publish_task(gc_task);
        rq
    }

    /// Account, EEVDF peer-preempt probe, and backup deadline under one lock.
    ///
    /// Returns `(elapsed, should_preempt, peer_wins, next_rel_on_probe_fail)`.
    #[cfg(all(feature = "preempt", feature = "sched_eevdf"))]
    fn account_peer_probe_and_next_deadline(
        &self,
        now_ns: u64,
        curr: &KtaskRef,
        is_idle: bool,
    ) -> (u64, bool, bool, Option<u64>) {
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
        let should_preempt = if is_idle || elapsed == 0 {
            false
        } else {
            sched.update_current(curr, elapsed)
        };
        let peer_wins = sched.peer_preempts_curr();
        let next_rel = if !peer_wins && !is_idle {
            sched.next_preemption_ns(curr)
        } else {
            None
        };
        (elapsed, should_preempt, peer_wins, next_rel)
    }

    /// Advances the account epoch and returns elapsed wall time.
    ///
    /// Caller must hold [`Self::scheduler`] so remote wake flush and local
    /// timer accounting cannot race on this field.
    pub(super) fn advance_account_epoch(last_accounted_ns: &AtomicU64, now_ns: u64) -> u64 {
        // The scheduler lock serializes this load/store with scheduler state;
        // Relaxed supplies interior mutability, not synchronization.
        let previous_ns = last_accounted_ns.load(Ordering::Relaxed);
        let elapsed = if previous_ns == 0 {
            0
        } else {
            now_ns.saturating_sub(previous_ns)
        };
        last_accounted_ns.store(now_ns, Ordering::Relaxed);
        elapsed
    }

    /// Charges pending wall time to the RQ's running entity before placement.
    ///
    /// NOHZ may leave a lone task without timer IRQs for a long time. Wakes and
    /// new publishes must flush that elapsed time into EEVDF `curr` before
    /// computing system V / PLACE_LAG.
    pub(super) fn flush_running_runtime(&self, now_ns: u64) {
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
        if elapsed == 0 {
            return;
        }
        #[cfg(feature = "sched_eevdf")]
        sched.account_curr_elapsed(elapsed);
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = elapsed;
        }
    }

    /// Publish a newly runnable task onto this run queue's scheduler.
    ///
    /// This is the only path that should call `Scheduler::add_task` from ktask.
    fn publish_task(&self, task: KtaskRef) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        #[cfg(feature = "smp")]
        self.charge_rq_load(&task);
        #[cfg(feature = "smp")]
        self.charge_home(&task);
        self.scheduler.lock().add_task(task);
    }

    /// Enqueue a non-current ready task onto this run queue (unblock / migrate-in).
    ///
    /// This is the only path that should call `Scheduler::enqueue_task` from ktask.
    /// Running tasks leave through `leave_current` on the yield/preempt/block/migrate/exit paths.
    #[cfg(feature = "smp")]
    pub(super) fn enqueue_task(&self, task: KtaskRef) {
        self.enqueue_ready_task(task, false);
    }

    /// Enqueue a ready task, optionally arming WF_SYNC in the same lock as
    /// NEXT_BUDDY nomination (Linux `ttwu` / `check_preempt_wakeup`).
    fn enqueue_ready_task(&self, task: KtaskRef, is_wake_sync: bool) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        #[cfg(feature = "smp")]
        self.charge_rq_load(&task);
        #[cfg(feature = "smp")]
        self.charge_home(&task);
        let now_ns = khal::time::monotonic_time_nanos();
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
        #[cfg(feature = "sched_eevdf")]
        sched.account_curr_elapsed(elapsed);
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = elapsed;
        }
        sched.enqueue_task(task);
        #[cfg(feature = "sched_eevdf")]
        if is_wake_sync {
            sched.mark_sync_wake_preempt();
        }
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = is_wake_sync;
        }
    }

    /// Attach a local helper task and switch to it without enqueueing.
    ///
    /// Used for per-CPU helpers such as the affinity migration task that are
    /// entered via `switch_to` rather than the ready queue. Under EEVDF, install
    /// the helper into `curr` here so later peer-preempt probes see it; idle
    /// never takes this path.
    #[cfg(feature = "smp")]
    fn switch_to_local(&self, prev_task: CurrentTask, next_task: KtaskRef) {
        #[cfg(feature = "smp")]
        self.attach_owner(&next_task);
        #[cfg(feature = "sched_eevdf")]
        self.scheduler.lock().sync_running_curr(&next_task);
        self.switch_to(prev_task, next_task);
    }

    /// Puts target task into current run queue with `Ready` state
    /// if its state matches `current_state` (except idle task).
    ///
    /// Used for non-current tasks (unblock). Running tasks use `leave_current`.
    /// `is_wake_sync` arms EEVDF WF_SYNC in the same scheduler lock as enqueue.
    ///
    /// Returns whether the transition was rejected, enqueued immediately, or
    /// handed to the CPU completing the task's switch-out.
    pub(super) fn put_task_with_state(
        &self,
        task: KtaskRef,
        current_state: TaskState,
        is_wake_sync: bool,
        resched: bool,
    ) -> WakeEnqueue {
        #[cfg(not(feature = "smp"))]
        let _ = resched;
        // If the task's state matches `current_state`, set its state to `Ready` and
        // put it back to the run queue (except idle task).
        if task.transition_state(current_state, TaskState::Ready) && !task.is_idle() {
            #[cfg(feature = "smp")]
            {
                // Dekker handoff with `clear_prev_task_on_cpu`: store pending
                // flags, then load `on_cpu`. The matching side stores
                // `on_cpu=false` then swaps the flags. SeqCst atomics plus the
                // store→load fence in `arm_wake_enqueue` / below; SeqCst
                // store then SeqCst load of another location is not enough.
                self.attach_owner(&task);
                task.arm_wake_enqueue(is_wake_sync, resched);
                if task.on_cpu() {
                    return WakeEnqueue::Deferred;
                }
                if task.take_wake_enqueue().is_none() {
                    return WakeEnqueue::Deferred;
                }
            }
            // TODO: priority
            self.enqueue_ready_task(task, is_wake_sync);
            WakeEnqueue::Enqueued
        } else {
            WakeEnqueue::Rejected
        }
    }

    /// Core reschedule subroutine.
    /// Pick the next task to run and switch to it.
    fn resched(&self) {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.cpu_id).resched);
        let next = self
            .scheduler
            .lock()
            .pick_next_task()
            .unwrap_or_else(|| current_idle_task().clone());
        #[cfg(feature = "smp")]
        let next = if next.is_idle() && self.idle_pull() {
            self.scheduler.lock().pick_next_task().unwrap_or(next)
        } else {
            next
        };
        #[cfg(feature = "sched_stat")]
        if next.is_idle() {
            sched_stat_inc(&sched_stat_cpu(self.cpu_id).pick_idle);
        }
        assert!(
            next.is_ready(),
            "next {} is not ready: {:?}",
            next.id_name(),
            next.state()
        );
        self.switch_to(current(), next);
    }

    fn switch_to(&self, prev_task: CurrentTask, next_task: KtaskRef) {
        // Make sure that IRQs are disabled by kernel guard or other means.
        assert!(
            !karch::local_irq_enabled(),
            "IRQs must be disabled during scheduling"
        );
        #[cfg(feature = "smp")]
        debug_assert_eq!(
            next_task.cpu_id(),
            self.cpu_id,
            "next task {} owner cpu {} != run queue {}",
            next_task.id_name(),
            next_task.cpu_id().as_usize(),
            self.cpu_id.as_usize()
        );
        trace!(
            "context switch: {} -> {}",
            prev_task.id_name(),
            next_task.id_name()
        );

        let now_ns = khal::time::monotonic_time_nanos();

        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        account_execution_resumed(next_task.execution_context());
        let program_next_deadline = |rq: &Self, next: &KtaskRef, now: u64| {
            let next_rel = {
                let sched = rq.scheduler.lock();
                rq.last_accounted_ns.store(now, Ordering::Relaxed);
                if next.is_idle() {
                    None
                } else {
                    sched.next_preemption_ns(next)
                }
            };
            // Immediate requests must arm pending on the *incoming* task: at
            // this point `current()` is still the outgoing task.
            program_sched_deadline_for(Some(next), now, next_rel);
            refresh_execution_tick_deadline_for_task(next);
            rearm_local_timer();
        };

        if prev_task.ptr_eq(&next_task) {
            #[cfg(feature = "sched_stat")]
            sched_stat_inc(&sched_stat_cpu(this_cpu_id()).switch_same);
            program_next_deadline(self, &next_task, now_ns);
            return;
        }

        let cpu_id = this_cpu_id();

        // Fire the context switch tracepoint.
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(cpu_id).switch);
        fire_context_switch(prev_task.trace_id(), next_task.trace_id());

        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        {
            next_task.set_on_cpu(true);
            next_task.set_on_cpu_mask_bit(this_cpu_id());
        }

        if let Some(runtime) = next_task.user_runtime() {
            // Publish the CPU in the next mm before switching hardware state
            // so concurrent flushers can over-target but never miss this CPU.
            // We intentionally do not clear the previous mm here: code after
            // `switch_to()` runs only when the old task is scheduled again, so
            // eager clear-after-switch is not a sound timing point.
            runtime.set_user_mm_resident_cpu(cpu_id);
        }

        {
            if let Some(runtime) = prev_task.user_runtime() {
                runtime.on_leave()
            }
        }

        // Program the schedule timer for the incoming task before switching.
        program_next_deadline(self, &next_task, now_ns);

        // SAFETY: scheduling owns both task contexts here, IRQs are disabled,
        // and the percpu scheduler/task pointers being updated are local to
        // the current CPU.
        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            #[cfg(target_arch = "aarch64")]
            if let Some(root) = next_task
                .user_runtime()
                .and_then(|runtime| runtime.switch_page_table_root())
            {
                (*next_ctx_ptr).set_page_table_root(root);
            }

            // Store the weak pointer of **prev_task** in percpu variable `PREV_TASK`.
            #[cfg(feature = "smp")]
            {
                *PREV_TASK.shared_current().lock() = Arc::downgrade(&prev_task);
            }

            // The strong reference count of `prev_task` will be decremented by 1,
            // but won't be dropped until `gc_entry()` is called.
            assert!(Arc::strong_count(&prev_task) > 1);
            assert!(Arc::strong_count(&next_task) >= 1);

            let suspended_exception = khal::context::suspend_active_exception_context();
            CurrentTask::set_current(prev_task, next_task);

            (*prev_ctx_ptr).switch_to(&*next_ctx_ptr);
            khal::context::resume_active_exception_context(suspended_exception);
            if let Some(runtime) = current().user_runtime() {
                runtime.on_enter();
            }

            // Current it's **next_task** running on this CPU, clear the `prev_task`'s `on_cpu` field
            // to indicate that it has finished its scheduling process and no longer running on this CPU.
            #[cfg(feature = "smp")]
            clear_prev_task_on_cpu(self);
        }
    }
}

/// Clear the `on_cpu` field of previous task running on this CPU.
#[cfg(feature = "smp")]
pub(crate) unsafe fn clear_prev_task_on_cpu(current_rq: &RunQueue) {
    // SAFETY: this is called on the CPU that owns `PREV_TASK`, after the
    // context switch completed and before another previous-task record is
    // installed.
    let prev_task = PREV_TASK
        .shared_current()
        .lock()
        .clone()
        .upgrade()
        .expect("Invalid prev_task pointer or prev_task has been dropped");
    prev_task.set_on_cpu(false);
    // Matching Dekker store-load fence; see `TaskInner::arm_wake_enqueue`.
    core::sync::atomic::fence(Ordering::SeqCst);

    // Matching Dekker load: SeqCst swap after the SeqCst `on_cpu=false`
    // store. If the waker deferred because it still saw `on_cpu`, this swap
    // observes the pending flags and finishes the enqueue. If this swap sees
    // 0, the waker already claimed the flags (or never armed) and will enqueue.
    let Some((is_wake_sync, resched)) = prev_task.take_wake_enqueue() else {
        return;
    };
    debug_assert!(
        prev_task.is_ready(),
        "pending wake task {} is not ready: {:?}",
        prev_task.id_name(),
        prev_task.state()
    );

    let target_cpu = prev_task.cpu_id();
    if target_cpu == current_rq.cpu_id {
        current_rq.enqueue_ready_task(prev_task, is_wake_sync);
        #[cfg(feature = "preempt")]
        if resched {
            request_resched_on(current_rq);
        }
    } else {
        let target_rq = get_run_queue(target_cpu.as_usize());
        target_rq.enqueue_ready_task(prev_task, is_wake_sync);
        #[cfg(feature = "preempt")]
        if resched {
            request_resched_on(target_rq);
        }
    }
    #[cfg(not(feature = "preempt"))]
    let _ = resched;
}
