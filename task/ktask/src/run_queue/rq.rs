// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Leave, pick, switch, resched kick, and run-queue operations.

#[cfg(unittest)]
#[path = "../tests/rcu_switch.rs"]
mod tests_rcu_switch;

use alloc::sync::Arc;
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU64, Ordering};

use kcpu_id_map::LogicalCpuId;
use khal::percpu::this_cpu_id;
use ksched::{Decision, EnqueueKind, Nice, UpdateKind};
use kspin::{BaseGuard, SpinNoIrqGuard, SpinRaw};

#[cfg(feature = "smp")]
use super::get_run_queue;
#[cfg(feature = "smp")]
use super::migrate::spawn_affinity_migration_task;
#[cfg(feature = "smp")]
use super::stats::record_remote_resched;
#[cfg(feature = "sched_stat")]
use super::stats::{sched_stat_cpu, sched_stat_inc};
use super::{
    CurrentRunQueueRef, KRunQueueRef, PREV_TASK, RunQueue, WakeEnqueue, current_wait_for_exit, gc,
    with_exited_tasks,
};
use crate::{
    KtaskRef, Scheduler, TaskInner,
    api::{current, program_sched_deadline, program_sched_deadline_for, rearm_local_timer},
    current_may_uninit,
    task::{CurrentTask, TaskState},
    tracing_hooks::{fire_context_switch, fire_task_wakeup},
};

pub(super) fn request_resched_on(rq: &RunQueue) {
    let cpu_id = rq.cpu_id;
    if cpu_id == this_cpu_id() {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(cpu_id).local_resched);
        current().set_preempt_pending(true);
        // A newly runnable peer may need an earlier deadline than the lone
        // runner, whose schedule timer may currently be disarmed. The class
        // view is taken even for an idle runner: a class can have a pending
        // event without a runnable task (a throttled deadline task waiting for
        // its next period), and an idle CPU gets no other timer IRQ to re-arm
        // it. The idle class reports no runner event of its own.
        let now_ns = khal::time::monotonic_time_nanos();
        let next_rel = rq.scheduler.lock().next_event_ns();
        program_sched_deadline(now_ns, next_rel);
        rearm_local_timer();
    } else {
        #[cfg(feature = "smp")]
        {
            let resched_result = request_remote_resched(cpu_id);
            record_remote_resched(cpu_id, resched_result.is_err());
        }
    }
}

#[cfg(feature = "smp")]
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
    #[cfg(feature = "smp")]
    {
        if !task.on_cpu() {
            return;
        }
        let cpu_id = task.cpu_id();
        if cpu_id == this_cpu_id() {
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
        record_remote_resched(cpu_id, kick_result.is_err());
    }
    #[cfg(not(feature = "smp"))]
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
    pub(crate) fn add_task(&mut self, task: KtaskRef) {
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
        let now_ns = khal::time::monotonic_time_nanos();
        let decision = self.inner.publish_task(task, EnqueueKind::New, now_ns);
        self.inner.kick(decision);
    }

    /// Unblock one task by inserting it into the run queue.
    ///
    /// This function does nothing if the task is not in [`TaskState::Blocked`],
    /// which means the task is already unblocked by other cores.
    pub(crate) fn unblock_task(&mut self, task: KtaskRef, resched: bool) {
        // Wakeups can originate in IRQ context; never allocate a task-name string.
        let task_id = task.trace_id();
        // Try to change the state of the task from `Blocked` to `Ready`,
        // if successful, the task will be put into this run queue,
        // otherwise, the task is already unblocked by other cores.
        // Note:
        // target task can not be insert into the run queue until it finishes its scheduling process.
        // Account + PLACE + WF_SYNC share one scheduler lock so a remote CPU
        // cannot leave/pick between flush and nominate (HEAD had no flush).
        let is_wake_sync = current_may_uninit().is_some_and(|c| c.is_wake_sync());
        let (wake_enqueue, decision) =
            self.inner
                .put_task_with_state(task, TaskState::Blocked, is_wake_sync, resched);
        if wake_enqueue != WakeEnqueue::Rejected {
            // Since now, the task to be unblocked is in the `Ready` state.
            let cpu_id = self.inner.cpu_id;
            debug!(
                "task unblock: trace_id={task_id} on run_queue {}",
                cpu_id.as_usize()
            );
            #[cfg(feature = "sched_stat")]
            sched_stat_inc(&sched_stat_cpu(cpu_id).unblock_task);

            // Fire the task wakeup tracepoint.
            fire_task_wakeup(task_id);

            if let Some(decision) = decision
                && resched
            {
                self.inner.kick(decision);
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
    pub(crate) fn account_current_runtime(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, false);
    }

    /// Linux `entity_tick`: account, then same-class tick preemption.
    ///
    /// Only the due schedule slot uses this. Soft/periodic IRQs must not
    /// re-evaluate pick — that ping-ponged WF_SYNC later-deadline handoffs.
    pub(crate) fn account_sched_tick(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, true);
    }

    fn account_runtime(&mut self, now_ns: u64, is_sched_tick: bool) {
        let curr = &self.current_task;
        let is_idle = curr.is_idle();
        let kind = if is_sched_tick {
            UpdateKind::SchedTick
        } else {
            UpdateKind::Account
        };
        let (elapsed, should_preempt) = {
            let elapsed = RunQueue::advance_account_epoch(&self.inner.last_accounted_ns, now_ns);
            let should_preempt = self.inner.with_sched(|s| s.update(now_ns, kind)).resched;
            (elapsed, should_preempt)
        };
        self.apply_runtime_account_side_effects(elapsed, is_idle, should_preempt);
    }

    fn apply_runtime_account_side_effects(
        &mut self,
        elapsed: u64,
        is_idle: bool,
        should_preempt: bool,
    ) {
        if !is_idle {
            #[cfg(feature = "sched_stat")]
            if should_preempt && elapsed > 0 {
                sched_stat_inc(&sched_stat_cpu(self.inner.cpu_id).tick_preempt);
            }
        }
        #[cfg(not(feature = "sched_stat"))]
        let _ = elapsed;
        if should_preempt {
            self.current_task.set_preempt_pending(true);
        }
    }

    /// Recomputes the per-CPU schedule deadline.
    ///
    /// Takes the class view even when the runner is idle: a class can still
    /// have a pending event with no runnable task, and dropping it would
    /// strand that event because an idle CPU gets no further timer IRQ. The
    /// idle class itself reports no runner event, so this only keeps events
    /// that some class genuinely asked for.
    pub(crate) fn refresh_sched_deadline(&mut self, now_ns: u64) {
        let next_rel = self.inner.with_sched(|s| s.next_event_ns());
        program_sched_deadline(now_ns, next_rel);
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub(crate) fn yield_current(&mut self) {
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
        let (_, pull_if_empty) = self.inner.leave_and_publish(|s| s.yield_current(now_ns));
        self.inner.resched(pull_if_empty, TaskState::Ready);
    }

    /// Migrate the current task to a new run queue matching its CPU affinity and reschedule.
    /// This function will spawn a new `migration_task` to perform the migration, which will set
    /// current task to `Ready` state and select a proper run queue for it according to its CPU affinity,
    /// switch to the migration task immediately after migration task is prepared.
    ///
    /// Note: the ownership if migrating task (which is current task) is handed over to the migration task,
    /// before the migration task inserted it into the target run queue.
    #[cfg(feature = "smp")]
    pub(crate) fn migrate_current(&mut self, migration_task: KtaskRef) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        trace!("task migrate: {}", curr.id_name());
        assert!(curr.is_running());

        // Migration helper already owns a strong ref (spawned with curr.clone()).
        // Deactivate on the source RQ before departure; destination `enqueue_task`
        // then applies PLACE_LAG via `needs_place`.
        self.inner.with_sched(|s| s.migrate_current(now_ns));

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Migration helper is switched to without enqueue; attach ownership first.
        self.inner
            .switch_to_local(current(), migration_task, TaskState::Ready);
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
    pub(crate) fn preempt_resched(&mut self) {
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

        if can_preempt && affinity_ok {
            let is_idle = self.current_task.is_idle();
            let (elapsed, decision) = {
                let elapsed =
                    RunQueue::advance_account_epoch(&self.inner.last_accounted_ns, now_ns);
                let decision = self
                    .inner
                    .with_sched(|s| s.update(now_ns, UpdateKind::WakeProbe));
                (elapsed, decision)
            };
            self.apply_runtime_account_side_effects(elapsed, is_idle, decision.resched);
            if !decision.resched {
                self.current_task.set_preempt_pending(false);
                // Keep every class event even when the runner is idle: a
                // class can have a pending event with nothing runnable, and
                // an idle CPU has no other timer IRQ to re-arm it.
                program_sched_deadline(now_ns, decision.next_event_ns);
                rearm_local_timer();
                return;
            }
        } else {
            self.account_current_runtime(now_ns);
        }

        let curr = &self.current_task;
        if can_preempt {
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
            let (_, pull_if_empty) = self.inner.leave_and_publish(|s| s.preempt_current(now_ns));
            self.inner.resched(pull_if_empty, TaskState::Ready);
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub(crate) fn exit_current(&mut self, exit_code: i32) -> ! {
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
            curr.notify_exit(exit_code);
            #[cfg(feature = "smp")]
            self.inner.discharge_home(curr);

            // SAFETY: `exit_current` runs under
            // `current_run_queue::<NoPreemptIrqSave>()`, so IRQs and
            // preemption are disabled while touching this CPU's exited list
            // and waking GC. The Arc returned by `exit_current` is the one
            // that goes on `EXITED_TASKS` (`refactor_ksched`); do not also
            // clone `curr` onto that list.
            let (exited, pull_if_empty) = self.inner.leave_and_publish(|s| s.exit_current(now_ns));
            with_exited_tasks(|tasks| tasks.push_back(exited));
            current_wait_for_exit().wake();

            self.inner.resched(pull_if_empty, TaskState::Exited);
        }
        unreachable!("task exited!");
    }

    /// Block the current task and reschedule.
    ///
    /// Marks the current task `Blocked` and leaves the scheduler without requeue
    /// ([`ksched::LeaveReason::Block`]).
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
    ///    the context switch. The into_raw current slot, `Scheduler.curr`, and
    ///    the caller's clone (e.g. from [`crate::future::block_on`]) are three
    ///    strong refs (`strong_count > 2`) before leave. Policy block does not
    ///    requeue, so that clone keeps the task alive until the wait queue
    ///    takes over. Do not drop the clone thinking the current slot counts
    ///    as the extra ref.
    /// 4. The wait-queue lock is released inside this function before rescheduling.
    ///
    /// # Panics
    ///
    /// Panics if `Arc::strong_count(current) <= 2` (missing caller-owned ref).
    /// This is a hard scheduler invariant: there is no recovery path. New
    /// wait-queue / timer callers must clone current before entry — see the
    /// `blocked_resched_survives_with_caller_owned_ref` unittest for a
    /// `block_on`-free template. `#[track_caller]` points the panic at the
    /// call site.
    #[track_caller]
    pub(crate) fn blocked_resched(&mut self, mut woke: SpinNoIrqGuard<'_, bool>) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        assert!(curr.is_running());
        assert!(!curr.is_idle());
        // we must not block current task with preemption disabled.
        // Current expected preempt count is 2 for `NoPreemptIrqSave` because we also hold
        // the `woke` SpinNoIrqGuard lock here.
        assert!(curr.can_preempt(2));
        assert!(!curr.rcu_reader.is_reading(), "blocking in RCU read scope");
        // CurrentTask slot + Scheduler.curr + caller clone (e.g. block_on).
        let strong = Arc::strong_count(curr);
        assert!(
            strong > 2,
            "blocked_resched: strong_count={strong} (need > 2); into_raw current slot and \
             Scheduler.curr count as 2 — caller must hold another KtaskRef across switch (see \
             block_on / blocked_resched rustdoc)"
        );

        // Deactivate while still Running / current, before any concurrent wake
        // can requeue us. Fair schedulers snapshot lag for later enqueue PLACE_LAG.
        let (_, pull_if_empty) = self.inner.leave_and_publish(|s| s.block_current(now_ns));

        // Mark the task as blocked, this has to be done before adding it to the wait queue
        // while holding the lock of the wait queue.
        curr.set_state(TaskState::Blocked);
        *woke = false;
        drop(woke);

        // Current task's state has been changed to `Blocked` and added to the wait queue.
        // Note that the state may have been set as `Ready` in `unblock_task()`,
        // see `unblock_task()` for details.

        debug!("task block: {}", curr.id_name());
        self.inner.resched(pull_if_empty, TaskState::Blocked);
    }

    pub(crate) fn set_current_priority(&mut self, prio: isize) -> bool {
        let now_ns = khal::time::monotonic_time_nanos();
        let Some(decision) = self.inner.set_fair_nice(&self.current_task, prio, now_ns) else {
            return false;
        };
        self.inner.kick(decision);
        true
    }
}

impl<G: BaseGuard> KRunQueueRef<'_, G> {
    pub(crate) fn set_task_priority(&mut self, task: &KtaskRef, prio: isize) -> bool {
        let now_ns = khal::time::monotonic_time_nanos();
        let Some(decision) = self.inner.set_fair_nice(task, prio, now_ns) else {
            return false;
        };
        self.inner.kick(decision);
        true
    }
}

impl RunQueue {
    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    pub(super) fn new(cpu_id: LogicalCpuId, idle: KtaskRef) -> Self {
        let gc_task = gc::spawn_gc_task(cpu_id);

        let now_ns = khal::time::monotonic_time_nanos();
        let scheduler = Scheduler::new(idle, now_ns);
        let rq = Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
            last_accounted_ns: AtomicU64::new(now_ns),
            #[cfg(feature = "smp")]
            nr_running: AtomicUsize::new(0),
            #[cfg(feature = "smp")]
            nr_home: AtomicUsize::new(0),
        };
        // Publish through the ownership-aware entry so secondary-CPU gc tasks
        // do not keep the default `cpu_id == 0`.
        rq.publish_task(gc_task, EnqueueKind::New, now_ns);
        rq
    }

    pub(crate) fn with_sched<R>(&self, f: impl FnOnce(&mut Scheduler) -> R) -> R {
        let mut sched = self.scheduler.lock();
        let r = f(&mut sched);
        #[cfg(feature = "smp")]
        self.store_load(Self::load_count(&sched));
        r
    }

    /// `nr_queued` plus the non-idle runner, matching `refactor_ksched`.
    #[cfg(feature = "smp")]
    fn load_count(sched: &Scheduler) -> usize {
        sched.nr_queued() + usize::from(sched.current().is_some_and(|t| !t.is_idle_class()))
    }

    #[cfg(feature = "smp")]
    fn store_load(&self, n: usize) {
        self.nr_running.store(n, Ordering::Relaxed);
    }

    /// Leave the runner and report whether the ready tree is empty in the same hold.
    fn leave_and_publish<R>(&self, leave: impl FnOnce(&mut Scheduler) -> R) -> (R, bool) {
        self.with_sched(|s| {
            let r = leave(s);
            (r, s.nr_queued() == 0)
        })
    }

    /// Request a preempt probe on this RQ's CPU. Local: always set
    /// `need_resched` and re-arm the timer. Remote: IPI only.
    ///
    /// `Decision` is unused: the target CPU re-probes in `preempt_resched`.
    /// Waker-side `next_event_ns` must not program dest's timer.
    pub(crate) fn kick(&self, decision: Decision) {
        let _ = decision;
        request_resched_on(self);
    }

    /// Install a runner that did not come from `pick_next` (boot `main`, AP idle,
    /// or the affinity migration helper).
    pub(super) fn install_running(&self, task: &KtaskRef) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.with_sched(|s| s.install_current(task.clone(), now_ns));
        self.last_accounted_ns.store(now_ns, Ordering::Relaxed);
    }

    /// Store a Fair nice value. `None` when `prio` is out of range or the
    /// task is the Idle class.
    ///
    /// Reads the current attributes and replaces only `nice`, so this path
    /// never rewrites `policy`, the RT priority or the deadline parameters. A
    /// non-Fair task therefore keeps its class and its reservation; the new
    /// nice value takes effect the next time it returns to Fair.
    pub(super) fn set_fair_nice(
        &self,
        task: &KtaskRef,
        prio: isize,
        now_ns: u64,
    ) -> Option<Decision> {
        let nice = Nice::new(prio as i32).ok()?;
        self.with_sched(|s| {
            let mut attr = s.attr(task);
            attr.nice = nice;
            s.set_attr(task, attr, now_ns)
        })
        .ok()
    }

    /// Advances the account epoch and returns elapsed wall time.
    ///
    /// `last_accounted_ns` is a best-effort Relaxed timestamp. Callers sample
    /// it outside [`Self::scheduler`]; a race only affects diagnostic
    /// elapsed, not scheduler state.
    pub(super) fn advance_account_epoch(last_accounted_ns: &AtomicU64, now_ns: u64) -> u64 {
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
        let _elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
        let _ = self.with_sched(|s| s.update(now_ns, UpdateKind::Account));
    }

    /// Place `task` onto this queue's policy and take ownership.
    ///
    /// Spawn, wake, idle-pull, and affinity migrate-in all go through here so
    /// `cpu_id` / `nr_home` move with the enqueue. Callers must not
    /// `Scheduler::enqueue` a published task without this.
    pub(super) fn publish_task(&self, task: KtaskRef, kind: EnqueueKind, now_ns: u64) -> Decision {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        #[cfg(feature = "smp")]
        self.charge_home(&task);
        let _elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
        self.with_sched(|s| s.enqueue(task, kind, now_ns))
    }

    /// Enqueue a non-current ready task onto this run queue (migrate-in).
    ///
    /// Running tasks leave through yield / preempt / block / migrate / exit.
    #[cfg(feature = "smp")]
    pub(super) fn enqueue_task(&self, task: KtaskRef) -> Decision {
        let now_ns = khal::time::monotonic_time_nanos();
        self.publish_task(task, EnqueueKind::MigrateIn, now_ns)
    }

    /// Enqueue a ready task, optionally arming WF_SYNC in the same lock as
    /// NEXT_BUDDY nomination (Linux `ttwu` / `check_preempt_wakeup`).
    fn enqueue_ready_task(&self, task: KtaskRef, is_wake_sync: bool) -> Decision {
        let now_ns = khal::time::monotonic_time_nanos();
        self.publish_task(task, EnqueueKind::Wake { sync: is_wake_sync }, now_ns)
    }

    /// Attach a local helper task and switch to it without enqueueing.
    ///
    /// Used for per-CPU helpers such as the affinity migration task that are
    /// entered via `switch_to` rather than the ready queue. Under EEVDF, install
    /// the helper into `curr` here so later peer-preempt probes see it; idle
    /// never takes this path. The caller supplies its leave-decision state;
    /// do not infer it from mutable task state or assume all helpers migrate.
    #[cfg(feature = "smp")]
    fn switch_to_local(&self, prev_task: CurrentTask, next_task: KtaskRef, prev_state: TaskState) {
        #[cfg(feature = "smp")]
        self.attach_owner(&next_task);
        self.with_sched(|s| {
            s.install_current(next_task.clone(), khal::time::monotonic_time_nanos());
        });
        self.switch_to(prev_task, next_task, prev_state);
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
    ) -> (WakeEnqueue, Option<Decision>) {
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
                    return (WakeEnqueue::Deferred, None);
                }
                if task.take_wake_enqueue().is_none() {
                    return (WakeEnqueue::Deferred, None);
                }
            }
            let decision = self.enqueue_ready_task(task, is_wake_sync);
            (WakeEnqueue::Enqueued, Some(decision))
        } else {
            (WakeEnqueue::Rejected, None)
        }
    }

    /// Core reschedule subroutine.
    ///
    /// `pull_if_empty` is the ready-tree emptiness sampled in the same sched
    /// lock as leave. Re-taking the lock just to read `nr_queued` is avoided.
    /// `prev_state` records the leave decision; a concurrent wake may already
    /// have changed the outgoing task from Blocked to Ready before the switch.
    fn resched(&self, pull_if_empty: bool, prev_state: TaskState) {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.cpu_id).resched);
        let now_ns = khal::time::monotonic_time_nanos();
        #[cfg(feature = "smp")]
        if pull_if_empty {
            let _ = self.idle_pull();
        }
        #[cfg(not(feature = "smp"))]
        let _ = pull_if_empty;
        let next = self.with_sched(|s| s.pick_next(now_ns));
        #[cfg(feature = "sched_stat")]
        if next.is_idle() {
            sched_stat_inc(&sched_stat_cpu(self.cpu_id).pick_idle);
        }
        assert!(
            next.is_ready() || next.is_idle(),
            "next {} is not ready: {:?}",
            next.id_name(),
            next.state()
        );
        self.switch_to(current(), next, prev_state);
    }

    fn switch_to(&self, prev_task: CurrentTask, next_task: KtaskRef, prev_state: TaskState) {
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

        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        let program_next_deadline = |rq: &Self, next: &KtaskRef, now: u64| {
            rq.last_accounted_ns.store(now, Ordering::Relaxed);
            // The incoming idle runner reports no class event of its own, but
            // another class may still have one pending (see
            // `refresh_sched_deadline`), so the class view is taken
            // unconditionally.
            let next_rel = rq.with_sched(|s| s.next_event_ns());
            // Immediate requests must arm pending on the *incoming* task: at
            // this point `current()` is still the outgoing task.
            program_sched_deadline_for(Some(next), now, next_rel);
            rearm_local_timer();
        };

        // SAFETY: IRQs/preemption are excluded and prev_task is this CPU's
        // Arc-owned current task. Post-switch cleanup unlinks exited readers
        // before releasing the outgoing owner.
        unsafe { crate::rcu::note_context_switch(&prev_task, self.cpu_id) };
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
        fire_context_switch(
            prev_task.trace_id(),
            next_task.trace_id(),
            prev_state as u64,
        );

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

        let prev_ctx_ptr = prev_task.ctx_mut_ptr();
        let next_ctx_ptr = next_task.ctx_mut_ptr();

        #[cfg(target_arch = "aarch64")]
        if let Some(root) = next_task
            .user_runtime()
            .and_then(|runtime| runtime.switch_page_table_root())
        {
            // SAFETY: The scheduler owns next_task's inactive context;
            // IRQs are disabled and it cannot be run by another CPU here.
            unsafe { (*next_ctx_ptr).set_page_table_root(root) };
        }

        // The scheduler/GC also retains the outgoing task. Switch cleanup
        // releases its current reference only after leaving its stack.
        assert!(Arc::strong_count(&prev_task) > 1);
        assert!(Arc::strong_count(&next_task) >= 1);

        let suspended_exception = khal::context::suspend_active_exception_context();
        // SAFETY: IRQs and migration are disabled, prev_task is current, and
        // the scheduler owns the next task and transfers its retained reference.
        let (prev_owner, next_task_ptr) = unsafe { prev_task.prepare_switch(next_task) };
        {
            let mut previous = PREV_TASK.shared_current().lock();
            debug_assert!(previous.is_none());
            *previous = Some(prev_owner);
        }

        // SAFETY: Scheduling owns the outgoing context exclusively; its task
        // remains retained in PREV_TASK and cannot be reclaimed here.
        let prev_ctx = unsafe { &mut *prev_ctx_ptr };
        // SAFETY: next_task's retained context is initialized and distinct from
        // prev_ctx; scheduler ownership excludes concurrent context mutation.
        let next_ctx = unsafe { &*next_ctx_ptr };
        // SAFETY: IRQs/migration are excluded and the contexts are distinct.
        // PREV_TASK retains the outgoing task through the stack switch;
        // next_task_ptr transfers its owning reference to architecture current.
        unsafe { prev_ctx.switch_to(next_ctx, next_task_ptr) };
        khal::context::resume_active_exception_context(suspended_exception);
        if let Some(runtime) = current().user_runtime() {
            runtime.on_enter();
        }

        // SAFETY: the incoming task is now running with IRQs disabled. Use
        // this CPU's handoff, not `self`: this continuation may have migrated.
        unsafe { finish_task_switch() };
    }
}

/// Completes the outgoing task's handoff on the incoming task's stack.
///
/// # Safety
///
/// Call exactly once after an actual switch, on its destination CPU with
/// IRQs and migration disabled, before another handoff replaces `PREV_TASK`.
/// The outgoing stack must be inactive: clearing `on_cpu` permits remote
/// enqueue, and releasing the old current reference permits reclamation.
pub(crate) unsafe fn finish_task_switch() {
    let prev_task = PREV_TASK
        .shared_current()
        .lock()
        .take()
        .expect("context switch must supply the outgoing task owner");
    if prev_task.state() == TaskState::Exited {
        // SAFETY: this task cannot resume, its stack is inactive and current
        // already names the incoming task. IRQs/migration remain excluded;
        // PREV_TASK's transferred Arc retains its storage through unlinking.
        // Task-only scope admission excludes IRQ/NMI access to this reader.
        let had_abandoned_read = unsafe { prev_task.rcu_reader.finish_task_exit() };
        if had_abandoned_read {
            warn!(
                "task {} exited with an active RCU read scope",
                prev_task.id_name()
            );
        }
    }
    #[cfg(feature = "smp")]
    {
        // The caller excludes migration and runs on the destination
        // CPU. Its run queue may differ from the suspended caller's queue.
        let current_rq = super::current_run_queue_ref();
        clear_prev_task_on_cpu(current_rq, prev_task);
    }
    #[cfg(not(feature = "smp"))]
    drop(prev_task);
}

/// Releases the on-CPU marker and completes any deferred wake enqueue.
#[cfg(feature = "smp")]
fn clear_prev_task_on_cpu(current_rq: &RunQueue, prev_task: KtaskRef) {
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
    let is_local = target_cpu == current_rq.cpu_id;
    let target_rq = if is_local {
        current_rq
    } else {
        get_run_queue(target_cpu.as_usize())
    };
    let decision = target_rq.enqueue_ready_task(prev_task, is_wake_sync);
    if resched {
        target_rq.kick(decision);
    }
}
