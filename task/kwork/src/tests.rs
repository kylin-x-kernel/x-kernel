// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use kspin::{NoPreempt, SpinNoIrq};
use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    attach_flush_barrier, clear_delayed_reservation,
    raw::{process_one_bottom_half_pool_work, schedule_work_on, system_percpu_wq},
};

static WORK_RUNS: AtomicUsize = AtomicUsize::new(0);
static WORK_OBSERVED: AtomicUsize = AtomicUsize::new(0);
static STORE_BEFORE_QUEUE: AtomicUsize = AtomicUsize::new(0);
static SELF_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);
static NESTED_RUN_RESULT: AtomicUsize = AtomicUsize::new(0);
static SAME_QUEUE_WAIT_TARGET: SpinNoIrq<Option<ScheduledWork>> = SpinNoIrq::new(None);
static SAME_QUEUE_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);
static DYNAMIC_QUEUE_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_WORKER_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static POOL_SLOT_OBSERVED: AtomicUsize = AtomicUsize::new(0);

struct CountingTimerHandle {
    cancels: Arc<AtomicUsize>,
}

impl WorkqueueTimerHandle for CountingTimerHandle {
    fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::AcqRel);
    }
}

fn test_pool_binding() -> SystemPoolBinding {
    test_pool_binding_for_kind(SystemWorkQueueKind::Default)
}

fn test_pool_binding_for_kind(kind: SystemWorkQueueKind) -> SystemPoolBinding {
    SystemPoolBinding::for_kind_cpu(kind, WorkqueueHostIf::current_cpu_id())
        .expect("current CPU should have the requested worker pool")
}

/// RAII guard giving one test exclusive control of the current CPU's default
/// system pool.
///
/// Kernel preemption and reliable wake delivery let live system workers
/// preempt a test and drain the shared per-CPU pool mid-test. Freezing wakes
/// restores exclusive test control: wake plans are buffered, awake workers
/// re-block, and provider drain loops stop taking work. Dropping the guard
/// discards the buffered wakes, leaving the pool quiescent for tests that
/// drain work manually.
struct FrozenTestPool {
    binding: SystemPoolBinding,
    active: bool,
}

struct SameQueueWaitTargetGuard;

impl FrozenTestPool {
    fn freeze_current() -> Self {
        let binding = test_pool_binding();
        Self::freeze_binding(binding)
    }

    fn freeze_binding(binding: SystemPoolBinding) -> Self {
        binding.freeze_wakes_for_tests();
        Self {
            binding,
            active: true,
        }
    }

    /// Ends the freeze executing buffered wakes, so live workers drain the
    /// work queued while frozen. Tests whose final step is a blocking flush
    /// call this instead of relying on the discarding drop.
    fn release_and_flush(mut self) {
        self.binding.unfreeze_wakes_for_tests(false);
        self.active = false;
    }
}

impl Drop for FrozenTestPool {
    fn drop(&mut self) {
        if self.active {
            self.binding.unfreeze_wakes_for_tests(true);
        }
    }
}

impl SameQueueWaitTargetGuard {
    fn install(target: ScheduledWork) -> Self {
        *SAME_QUEUE_WAIT_TARGET.lock() = Some(target);
        Self
    }
}

impl Drop for SameQueueWaitTargetGuard {
    fn drop(&mut self) {
        *SAME_QUEUE_WAIT_TARGET.lock() = None;
    }
}

fn ensure_test_worker(binding: SystemPoolBinding, worker_id: WorkerId) {
    let mut pool_state = binding.pool().state.lock();
    if pool_state.workers[worker_id.as_usize()].state == WorkerState::Empty {
        core::assert!(pool_state.install_worker(worker_id.as_usize()));
    }
}

fn drain_one_test_work(worker_id: WorkerId) -> bool {
    drain_one_test_work_for_kind(SystemWorkQueueKind::Default, worker_id)
}

fn drain_one_test_work_for_kind(kind: SystemWorkQueueKind, worker_id: WorkerId) -> bool {
    let binding = test_pool_binding_for_kind(kind);
    ensure_test_worker(binding, worker_id);
    binding.run_one_work(worker_id)
}

fn test_pwq(queue: &WorkQueue) -> &SpinNoIrq<WorkQueuePoolState> {
    queue
        .pool_state_for_cpu(WorkqueueHostIf::current_cpu_id())
        .expect("current CPU should have a pool_workqueue")
}

fn count_work(_work: &ScheduledWork) {
    WORK_RUNS.fetch_add(1, Ordering::Relaxed);
}

fn observe_store_work(_work: &ScheduledWork) {
    WORK_OBSERVED.store(
        STORE_BEFORE_QUEUE.load(Ordering::Acquire),
        Ordering::Release,
    );
}

fn observe_worker_id_work(_work: &ScheduledWork) {
    let worker_id = WorkqueueTaskContextIf::current_work_context()
        .map(|context| context.worker_id().as_usize())
        .unwrap_or(usize::MAX);
    OBSERVED_WORKER_ID.store(worker_id, Ordering::Release);
}

fn observe_pool_worker_slot_work(work: &ScheduledWork) {
    let Some(context) = WorkqueueTaskContextIf::current_work_context() else {
        POOL_SLOT_OBSERVED.store(usize::MAX, Ordering::Release);
        return;
    };
    let observed = test_pool_binding().pool().state.lock().workers[context.worker_id().as_usize()]
        .current_work_key;
    POOL_SLOT_OBSERVED.store(usize::from(observed == work.key()), Ordering::Release);
}

fn self_flush_work(work: &ScheduledWork) {
    let result = work.flush();
    let code = match result {
        Err(WorkqueueError::SelfWait) => 2,
        Ok(_) => 1,
        Err(_) => 3,
    };
    SELF_WAIT_RESULT.store(code, Ordering::Release);
}

fn nested_run_work(_work: &ScheduledWork) {
    let ran_nested = usize::from(drain_one_test_work(WorkerId::new(0)));
    NESTED_RUN_RESULT.store(ran_nested, Ordering::Release);
}

fn flush_other_pending_work(_work: &ScheduledWork) {
    let target = SAME_QUEUE_WAIT_TARGET
        .lock()
        .as_ref()
        .cloned()
        .expect("same-pool wait target should be installed");
    let code = match target.flush() {
        Err(WorkqueueError::SelfWait) => 2,
        Ok(_) => 1,
        Err(_) => 3,
    };
    SAME_QUEUE_WAIT_RESULT.store(code, Ordering::Release);
}

fn flush_dynamic_queue_from_callback(queue: WorkQueueHandle) -> ScheduledWork {
    ScheduledWork::new(move |_work| {
        let code = match queue.flush() {
            Err(WorkqueueError::SelfWait) => 2,
            Ok(_) => 1,
            Err(_) => 3,
        };
        DYNAMIC_QUEUE_WAIT_RESULT.store(code, Ordering::Release);
    })
}

fn test_work(func: fn(&ScheduledWork)) -> ScheduledWork {
    ScheduledWork::new(func)
}

fn arm_delayed_for_tests(work: &DelayedScheduledWork, target: DelayedWorkTarget) -> WorkInstanceId {
    let pool_key = target
        .pool_key()
        .expect("test delayed target should have a pool key");
    let mut state = work.inner.state.lock();
    let mut work_state = work.scheduled().inner().state.lock();
    let seq = work_state.allocate_instance_id();
    work.scheduled().inner().done.reinit();
    work_state.set_delayed_pending(seq, pool_key);
    state
        .arm(target, seq)
        .expect("test delayed work should be idle");
    work.inner.done.reinit();
    seq
}

fn install_counting_timer_for_tests(
    work: &DelayedScheduledWork,
    instance_id: WorkInstanceId,
) -> Arc<AtomicUsize> {
    let cancels = Arc::new(AtomicUsize::new(0));
    let timer_generation = work.inner.state.lock().timer_generation();
    let timer_handle = Arc::new(CountingTimerHandle {
        cancels: cancels.clone(),
    });
    core::assert!(work.inner.state.lock().install_timer_handle(
        instance_id,
        timer_generation,
        timer_handle,
    ));
    cancels
}

fn mark_pending_for_tests(work: &ScheduledWork, binding: WorkQueuePoolBinding) {
    let mut work_state = work.inner().state.lock();
    let seq = work_state.allocate_instance_id();
    work_state.set_pending(seq, binding, WorkColor::DEFAULT);
}

fn mark_running_owner_for_tests(work: &ScheduledWork, owner: QueueOwner) {
    let binding = test_pool_binding();
    ensure_test_worker(binding, WorkerId::new(0));
    let work_binding = match owner {
        QueueOwner::Static(queue) => queue
            .select_pool_binding(None)
            .expect("test static queue should resolve a pool binding"),
        QueueOwner::Dynamic(queue) => queue
            .select_pool_binding(None)
            .expect("test dynamic queue should resolve a pool binding"),
    };
    let work_owner = work_binding.owner();
    let queue = work_owner.queue();
    let instance_id = work.inner().state.lock().allocate_instance_id();
    let worker_token =
        binding
            .pool()
            .state
            .lock()
            .start_running_work(0, work.key(), instance_id, Vec::new());
    {
        let mut work_state = work.inner().state.lock();
        work_state.set_running(
            instance_id,
            work_binding,
            WorkerId::new(0),
            worker_token,
            WorkColor::DEFAULT,
        );
    }
    let mut binding = test_pwq(queue).lock();
    binding.add_active();
    binding.start_running();
    binding.inc_in_flight(WorkColor::DEFAULT);
}

fn mark_running_for_tests(work: &ScheduledWork, queue: &'static WorkQueue) {
    mark_running_owner_for_tests(work, QueueOwner::Static(queue));
}

fn mark_canceling_for_tests(work: &ScheduledWork, queue: &'static WorkQueue) {
    mark_running_for_tests(work, queue);
    work.inner().state.lock().cancel_running();
}

#[def_test(serial)]
fn test_queue_work_suppresses_duplicate_pending_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueue::new("test");
    let queue = Box::leak(Box::new(queue));
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::AlreadyQueued);
    assert_eq!(queue.pending_len_for_tests(), 1);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(queue.pending_len_for_tests(), 0);
}

#[def_test(serial)]
fn test_separate_scheduled_work_instances_are_independent() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test-template")));

    WORK_RUNS.store(0, Ordering::Relaxed);
    let first = ScheduledWork::new(count_work);
    let second = ScheduledWork::new(count_work);
    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    assert_eq!(queue.pending_len_for_tests(), 2);

    assert_eq!(first.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(queue.pending_len_for_tests(), 1);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(second.flush(), Ok(false));
}

#[def_test(serial)]
fn test_schedule_attrs_selects_long_system_queue() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    let binding = test_pool_binding();
    let scheduled = ScheduledWork::new(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(
        scheduled.schedule_with(ScheduleAttrs::long_system().on_cpu(cpu_id)),
        QueueWorkResult::Queued
    );
    assert!(binding.has_runnable_work());

    assert!(drain_one_test_work_for_kind(
        SystemWorkQueueKind::Default,
        WorkerId::new(0)
    ));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(scheduled.flush(), Ok(false));
}

#[def_test]
fn test_budgeted_poller_assist_runs_one_scheduled_round() {
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new("budgeted-assist-test", 1usize, 1usize, 4, move |_| {
        poll_count.fetch_add(1, Ordering::AcqRel);
        BudgetedPollProgress { has_more: false }
    });

    assert!(!poller.notify_irq_safe());
    poller.assist_once();

    assert_eq!(polls.load(Ordering::Acquire), 1);
    assert!(poller.is_idle_for_tests());
}

#[def_test]
fn test_budgeted_poller_notify_during_run_requests_followup() {
    let poller = BudgetedPoller::new("budgeted-missed-test", 1usize, 1usize, 4, |_| {
        BudgetedPollProgress { has_more: false }
    });

    poller.force_running_for_tests();
    assert!(poller.notify_irq_safe());
    assert!(poller.is_running_pending_for_tests());
    poller.finish_run_for_tests(false);
    assert!(poller.is_scheduled_for_tests());
}

#[def_test]
fn test_budgeted_poller_background_batch_honors_max_rounds() {
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new("budgeted-max-rounds-test", 1usize, 1usize, 2, move |_| {
        poll_count.fetch_add(1, Ordering::AcqRel);
        BudgetedPollProgress { has_more: true }
    });

    assert!(!poller.notify_irq_safe());
    poller.poll_background_batch_for_tests();

    assert_eq!(polls.load(Ordering::Acquire), 2);
    assert!(poller.is_scheduled_for_tests());
}

#[def_test(serial)]
fn test_budgeted_poller_worker_drains_notified_work() {
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new("budgeted-worker-test", 1usize, 1usize, 4, move |_| {
        poll_count.fetch_add(1, Ordering::AcqRel);
        BudgetedPollProgress { has_more: false }
    });

    poller.start().expect("budgeted poller should start");
    {
        let _pin = NoPreempt::new();
        let _frozen_pool = FrozenTestPool::freeze_current();
        assert!(poller.notify_irq_safe());
        assert!(drain_one_test_work(WorkerId::new(0)));
    }

    assert_eq!(polls.load(Ordering::Acquire), 1);
    assert!(poller.is_idle_for_tests());
    poller.destroy().expect("budgeted poller should destroy");
}

#[def_test(serial)]
fn test_budgeted_poller_notify_recovers_after_queue_full() {
    let filler_queue = Box::leak(Box::new(WorkQueue::new("budgeted-full-filler")));
    let mut filler = Vec::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new("budgeted-queue-full-test", 1usize, 1usize, 4, move |_| {
        poll_count.fetch_add(1, Ordering::AcqRel);
        BudgetedPollProgress { has_more: false }
    });

    poller.start().expect("budgeted poller should start");
    {
        let _pin = NoPreempt::new();
        let _frozen_pool = FrozenTestPool::freeze_current();
        for _ in 0..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(filler_queue.queue_work(&work), QueueWorkResult::Queued);
            filler.push(work);
        }

        assert!(!poller.notify_irq_safe());
        assert!(poller.is_idle_for_tests());

        assert!(drain_one_test_work(WorkerId::new(0)));
        assert!(poller.notify_irq_safe());
        while drain_one_test_work(WorkerId::new(0)) {}
    }

    assert_eq!(polls.load(Ordering::Acquire), 1);
    assert!(poller.is_idle_for_tests());
    poller.destroy().expect("budgeted poller should destroy");
}

#[def_test(serial)]
fn test_budgeted_poller_scheduled_notify_retries_queueing() {
    let filler_queue = Box::leak(Box::new(WorkQueue::new("budgeted-retry-filler")));
    let mut filler = Vec::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new(
        "budgeted-scheduled-retry-test",
        1usize,
        1usize,
        4,
        move |_| {
            poll_count.fetch_add(1, Ordering::AcqRel);
            BudgetedPollProgress { has_more: false }
        },
    );

    poller.start().expect("budgeted poller should start");
    {
        let _pin = NoPreempt::new();
        let _frozen_pool = FrozenTestPool::freeze_current();
        poller.force_scheduled_for_tests();
        for _ in 0..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(filler_queue.queue_work(&work), QueueWorkResult::Queued);
            filler.push(work);
        }

        assert!(!poller.notify_irq_safe());
        assert!(poller.is_scheduled_for_tests());

        assert!(drain_one_test_work(WorkerId::new(0)));
        assert!(poller.notify_irq_safe());
        while drain_one_test_work(WorkerId::new(0)) {}
    }

    assert_eq!(polls.load(Ordering::Acquire), 1);
    assert!(poller.is_idle_for_tests());
    poller.destroy().expect("budgeted poller should destroy");
}

#[def_test(serial)]
fn test_budgeted_poller_followup_queue_failure_reopens_notify() {
    let filler_queue: &'static WorkQueue =
        Box::leak(Box::new(WorkQueue::new("budgeted-followup-filler")));
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = polls.clone();
    let poller = BudgetedPoller::new(
        "budgeted-followup-retry-test",
        1usize,
        1usize,
        1,
        move |_| {
            let previous = poll_count.fetch_add(1, Ordering::AcqRel);
            if previous == 0 {
                let filler = test_work(count_work);
                if filler_queue.queue_work(&filler) != QueueWorkResult::Queued {
                    panic!("filler work should occupy the released pending slot");
                }
                BudgetedPollProgress { has_more: true }
            } else {
                BudgetedPollProgress { has_more: false }
            }
        },
    );

    poller.start().expect("budgeted poller should start");
    {
        let _pin = NoPreempt::new();
        let _frozen_pool = FrozenTestPool::freeze_current();
        assert!(poller.notify_irq_safe());
        for _ in 1..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(filler_queue.queue_work(&work), QueueWorkResult::Queued);
        }

        assert!(drain_one_test_work(WorkerId::new(0)));
        assert_eq!(polls.load(Ordering::Acquire), 1);
        assert!(poller.is_idle_for_tests());

        assert!(drain_one_test_work(WorkerId::new(0)));
        assert!(poller.notify_irq_safe());
        while drain_one_test_work(WorkerId::new(0)) {}
    }

    assert_eq!(polls.load(Ordering::Acquire), 2);
    assert!(poller.is_idle_for_tests());
    poller.destroy().expect("budgeted poller should destroy");
}

#[def_test]
fn test_budgeted_poller_destroy_gates_later_notify_and_start() {
    let poller = BudgetedPoller::new("budgeted-destroy-test", 1usize, 1usize, 4, |_| {
        BudgetedPollProgress { has_more: false }
    });

    poller.destroy().expect("unstarted poller should destroy");

    assert!(!poller.notify_irq_safe());
    assert_eq!(poller.start(), Err(BudgetedPollerStartError::Destroyed));
    assert!(poller.is_idle_for_tests());
}

#[def_test(serial)]
fn test_shared_pool_drains_multiple_workqueue_bindings() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let first_queue = Box::leak(Box::new(WorkQueue::new("test-binding-first")));
    let second_queue = Box::leak(Box::new(WorkQueue::new("test-binding-second")));
    let first = test_work(count_work);
    let second = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(first_queue.queue_work(&first), QueueWorkResult::Queued);
    assert_eq!(second_queue.queue_work(&second), QueueWorkResult::Queued);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(first_queue.pending_len_for_tests(), 0);
    assert_eq!(second_queue.pending_len_for_tests(), 1);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
    assert_eq!(second_queue.pending_len_for_tests(), 0);
}

#[def_test(serial)]
fn test_schedule_attrs_queue_custom_static_workqueue() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue: &'static WorkQueue = Box::leak(Box::new(WorkQueue::new("test-schedule-static")));
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(
        work.schedule_with(ScheduleAttrs::queue(queue)),
        QueueWorkResult::Queued
    );
    assert!(queue.has_runnable_work_for_tests());
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_scheduled_work_convenience_queues_dynamic_workqueue() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-schedule-dynamic");
    let scheduled = ScheduledWork::new(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(scheduled.schedule_on_queue(&queue), QueueWorkResult::Queued);
    assert!(queue.has_runnable_work_for_tests());
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(scheduled.flush(), Ok(false));
}

#[def_test(serial)]
fn test_custom_workqueue_flush_resolves_every_cpu_binding() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-flush-bindings")));
    let bindings = WorkQueuePoolBinding::for_all_owner_cpus(QueueOwner::Static(queue))
        .expect("custom workqueue should resolve every default pool binding");

    assert_eq!(bindings.len(), kbuild_config::NR_CPUS);
}

#[def_test(serial)]
fn test_work_can_be_queued_again_after_idle() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn test_queued_work_survives_owner_handle_drop() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));

    WORK_RUNS.store(0, Ordering::Relaxed);
    {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    }

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_shared_pool_drains_dynamic_workqueue() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-dynamic");
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(queue.has_runnable_work_for_tests());
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert!(!queue.has_runnable_work_for_tests());
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_shared_pool_drain_records_worker_id() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(observe_worker_id_work);
    let worker_id = WorkerId::new(1);

    OBSERVED_WORKER_ID.store(usize::MAX, Ordering::Release);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(drain_one_test_work(worker_id));
    assert_eq!(OBSERVED_WORKER_ID.load(Ordering::Acquire), 1);
}

#[def_test(serial)]
fn test_pool_worker_slot_tracks_current_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(observe_pool_worker_slot_work);
    let worker_id = WorkerId::new(1);

    POOL_SLOT_OBSERVED.store(0, Ordering::Release);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(drain_one_test_work(worker_id));
    assert_eq!(POOL_SLOT_OBSERVED.load(Ordering::Acquire), 1);
    assert_eq!(
        test_pool_binding().pool().state.lock().workers[worker_id.as_usize()].current_work_key,
        0
    );
}

#[def_test(serial)]
fn test_zero_delay_work_queues_immediately() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::ZERO),
        QueueDelayedWorkResult::Queued
    );
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_cancel_delayed_work_disarms_pending_timer() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );
    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::AlreadyQueued
    );
    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(queue.pending_len_for_tests(), 0);
    assert!(!drain_one_test_work(WorkerId::new(0)));
}

#[def_test(serial)]
fn test_flush_work_ignores_timer_pending_delayed_work() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);
    let _instance_id = arm_delayed_for_tests(&work, DelayedWorkTarget::Static(queue));

    assert_eq!(work.scheduled().flush(), Ok(false));
    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_delayed_timer_reservation_blocks_immediate_inner_queue() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );
    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::ZERO),
        QueueDelayedWorkResult::AlreadyQueued
    );
    assert!(matches!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::DelayedPending
    ));

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Idle
    );
}

#[def_test(serial)]
fn test_delayed_timer_reservation_has_no_pool_queue_owner() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);
    let pool_key = DelayedWorkTarget::Static(queue)
        .pool_key()
        .expect("test delayed target should resolve to a pool key");

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );

    let work_state = work.scheduled().inner().state.lock();
    assert_eq!(work_state.status(), WorkStatus::DelayedPending);
    assert!(work_state.pending_binding_cloned().is_none());
    assert_eq!(work_state.pending_pool_key(), pool_key);
    drop(work_state);

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_static_system_delayed_target_uses_shared_pool() {
    let queue = system_wq();
    let work = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&work, DelayedWorkTarget::Static(queue));
    let result = DelayedWorkTarget::Static(queue).queue_reserved_work(work.scheduled(), seq);

    match result {
        QueueWorkResult::Queued => {
            work.cancel_sync()
                .expect("system delayed test work should sync cleanup");
        }
        QueueWorkResult::WorkerUnavailable => {
            clear_delayed_reservation(&work, seq).wake();
        }
        other => panic!("static system delayed target returned unexpected result: {other:?}"),
    }
}

#[def_test(serial)]
fn test_cancel_delayed_work_cancels_pending_after_timer_fire() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);
    let instance_id = arm_delayed_for_tests(&work, DelayedWorkTarget::Static(queue));

    work.fire_timer(instance_id);
    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Pending
    );

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
}

#[def_test(serial)]
fn test_timer_enqueue_failure_keeps_delayed_work_cancelable() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut queued = Vec::new();
    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        queued.push(work);
    }
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));

    delayed.fire_timer(seq);

    assert!(matches!(
        delayed.inner.state.lock().status,
        DelayedWorkStatus::Ready(ready_seq) if ready_seq == seq
    ));
    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
    queue.flush().expect("queued work should drain");
}

#[def_test(serial)]
fn test_custom_static_queue_on_cpu_uses_default_pool() {
    let queue = Box::leak(Box::new(WorkQueue::new("custom-static-on-cpu")));
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    let binding = queue
        .select_pool_binding(Some(cpu_id))
        .expect("custom static queue should bind to the requested default pool");
    let default_pool = TaskPoolBinding::default_for_cpu(cpu_id)
        .expect("current CPU default pool should be available")
        .pool();

    assert!(binding.owner().same_queue(queue));
    assert_eq!(binding.pool_key(), default_pool.key());
}

#[def_test(serial)]
fn test_flush_delayed_work_rejects_same_pool_before_queue() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let outer = test_work(count_work);
    let delayed = DelayedScheduledWork::new(count_work);
    let _seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));
    let _current = CurrentWorkGuard::enter(
        queue,
        test_pool_binding().pool().key(),
        WorkerId::new(0),
        WorkerExecutionToken::from_usize(1),
        &outer,
    );

    assert_eq!(delayed.flush(), Err(WorkqueueError::SelfWait));
    assert_eq!(queue.pending_len_for_tests(), 0);
    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_flush_delayed_work_keeps_ready_on_queue_failure() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut queued = Vec::new();
    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        queued.push(work);
    }
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));

    assert_eq!(delayed.flush(), Err(WorkqueueError::QueueFailed));
    assert!(matches!(
        delayed.inner.state.lock().status,
        DelayedWorkStatus::Ready(ready_seq) if ready_seq == seq
    ));
    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
    queue.flush().expect("queued work should drain");
}

#[def_test(serial)]
fn test_disable_work_blocks_queue_until_depth_reaches_zero() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    assert_eq!(work.disable(), CancelWorkResult::NotPending);
    assert_eq!(work.disable(), CancelWorkResult::NotPending);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Disabled);
    assert!(!work.enable());
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Disabled);
    assert!(work.enable());
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    work.flush().expect("enabled work should finish");
}

#[def_test(serial)]
fn test_timer_fire_clears_reservation_when_work_is_disabled() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&work, DelayedWorkTarget::Static(queue));
    let timer_generation = work.inner.state.lock().timer_generation();
    let target = work
        .inner
        .state
        .lock()
        .begin_timer_fire(seq, timer_generation)
        .expect("test delayed work should enter firing");

    work.scheduled().gate().lock().disable();
    assert_eq!(
        target.queue_reserved_work(work.scheduled(), seq),
        QueueWorkResult::Disabled
    );
    assert!(
        work.inner
            .state
            .lock()
            .finish_fire(seq, target, DelayedFireOutcome::Clear)
    );
    clear_delayed_reservation(&work, seq).wake();

    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Idle
    );
    assert!(work.enable());
}

#[def_test(serial)]
fn test_flush_disabled_delayed_work_clears_reserved_instance() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);
    let _ = arm_delayed_for_tests(&work, DelayedWorkTarget::Static(queue));
    work.scheduled().gate().lock().disable();

    assert_eq!(work.flush(), Err(WorkqueueError::QueueFailed));
    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Idle
    );
    assert_eq!(work.enable(), true);
    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::ZERO),
        QueueDelayedWorkResult::Queued
    );
    work.flush().expect("self-wait callback should finish");
}

#[def_test(serial)]
fn test_flush_delayed_work_cancels_timer_before_immediate_queue_attempt() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut queued = Vec::new();
    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        queued.push(work);
    }
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));
    let timer_cancels = install_counting_timer_for_tests(&delayed, seq);

    assert_eq!(delayed.flush(), Err(WorkqueueError::QueueFailed));
    assert_eq!(timer_cancels.load(Ordering::Acquire), 1);
    assert!(matches!(
        delayed.inner.state.lock().status,
        DelayedWorkStatus::Ready(ready_seq) if ready_seq == seq
    ));

    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
    queue.flush().expect("queued work should drain");
}

#[def_test(serial)]
fn test_mod_delayed_work_preserves_instance_and_ignores_stale_timer() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );
    let (seq, old_generation) = {
        let state = work.inner.state.lock();
        let seq = match state.status {
            DelayedWorkStatus::Pending(seq) => seq,
            other => panic!("unexpected delayed state before mod: {other:?}"),
        };
        (seq, state.timer_generation())
    };
    assert_eq!(work.scheduled().flush(), Ok(false));

    assert_eq!(
        queue.mod_delayed_work(&work, TimeSpan::from_secs(120)),
        QueueDelayedWorkResult::Queued
    );
    let new_generation = {
        let state = work.inner.state.lock();
        assert!(matches!(state.status, DelayedWorkStatus::Pending(pending) if pending == seq));
        state.timer_generation()
    };
    assert_ne!(old_generation, new_generation);

    work.fire_timer_with_generation(seq, old_generation);
    assert!(matches!(
        work.inner.state.lock().status,
        DelayedWorkStatus::Pending(pending) if pending == seq
    ));
    assert_eq!(queue.pending_len_for_tests(), 0);

    assert_eq!(
        queue.mod_delayed_work(&work, TimeSpan::ZERO),
        QueueDelayedWorkResult::Queued
    );
    assert_eq!(queue.pending_len_for_tests(), 1);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Idle
    );
    frozen_pool.release_and_flush();
}

#[def_test(serial)]
fn test_mod_delayed_work_zero_delay_cancels_timer_before_immediate_queue() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));
    let timer_cancels = install_counting_timer_for_tests(&delayed, seq);

    assert_eq!(
        queue.mod_delayed_work(&delayed, TimeSpan::ZERO),
        QueueDelayedWorkResult::Queued
    );
    assert_eq!(timer_cancels.load(Ordering::Acquire), 1);
    assert_eq!(delayed.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        delayed.scheduled().inner().state.lock().status(),
        WorkStatus::Pending
    );

    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
}

#[def_test(serial)]
fn test_mod_delayed_work_rearms_after_timer_queued_pending_work() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );
    let seq = match work.inner.state.lock().status {
        DelayedWorkStatus::Pending(seq) => seq,
        other => panic!("unexpected delayed state before timer fire: {other:?}"),
    };

    work.fire_timer(seq);
    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Pending
    );
    assert_eq!(queue.pending_len_for_tests(), 1);

    assert_eq!(
        queue.mod_delayed_work(&work, TimeSpan::from_secs(120)),
        QueueDelayedWorkResult::Queued
    );
    assert!(matches!(
        work.inner.state.lock().status,
        DelayedWorkStatus::Pending(_)
    ));
    {
        let work_state = work.scheduled().inner().state.lock();
        assert_eq!(work_state.status(), WorkStatus::DelayedPending);
        assert!(work_state.pending_binding_cloned().is_none());
    }
    assert_eq!(queue.pending_len_for_tests(), 0);

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
}

#[def_test(serial)]
fn test_mod_delayed_work_zero_delay_requeues_pool_pending_work() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = DelayedScheduledWork::new(count_work);

    assert_eq!(
        queue.queue_delayed_work(&work, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Queued
    );
    let seq = match work.inner.state.lock().status {
        DelayedWorkStatus::Pending(seq) => seq,
        other => panic!("unexpected delayed state before timer fire: {other:?}"),
    };

    work.fire_timer(seq);
    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Pending
    );
    assert_eq!(queue.pending_len_for_tests(), 1);

    assert_eq!(
        queue.mod_delayed_work(&work, TimeSpan::ZERO),
        QueueDelayedWorkResult::Queued
    );
    assert_eq!(work.inner.state.lock().status, DelayedWorkStatus::Idle);
    assert_eq!(
        work.scheduled().inner().state.lock().status(),
        WorkStatus::Pending
    );
    assert_eq!(queue.pending_len_for_tests(), 1);

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
}

#[def_test(serial)]
fn test_mod_delayed_work_zero_delay_keeps_ready_on_queue_full() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut queued = Vec::new();
    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        queued.push(work);
    }
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));

    assert_eq!(
        queue.mod_delayed_work(&delayed, TimeSpan::ZERO),
        QueueDelayedWorkResult::QueueFull
    );
    assert!(matches!(
        delayed.inner.state.lock().status,
        DelayedWorkStatus::Ready(ready_seq) if ready_seq == seq
    ));
    assert_eq!(delayed.cancel(), CancelWorkResult::CancelledPending);
    frozen_pool.release_and_flush();
    queue.flush().expect("queued work should drain");
}

#[def_test(serial)]
fn test_dynamic_workqueue_destroy_gate_rejects_new_work() {
    let queue = WorkQueueHandle::new("test-dynamic");
    let work = test_work(count_work);

    queue.queue().state.lock().is_destroying = true;

    assert_eq!(queue.queue_work(&work), QueueWorkResult::Disabled);
    assert!(!queue.has_runnable_work_for_tests());
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Idle);
}

#[def_test(serial)]
fn test_dynamic_queue_color_flush_ignores_later_work_color() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-color-flush");
    let first = test_work(count_work);
    let second = test_work(count_work);

    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    let flush_color = {
        let mut queue_state = test_pwq(queue.queue()).lock();
        let color = queue_state.advance_work_color();
        assert!(queue_state.begin_flush(color).is_some());
        color
    };
    let later_color = flush_color.next();

    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    {
        let queue_state = test_pwq(queue.queue()).lock();
        assert_eq!(queue_state.in_flight_for_tests(flush_color), 1);
        assert_eq!(queue_state.in_flight_for_tests(later_color), 1);
        assert!(queue_state.has_active_flush());
    }

    assert_eq!(first.cancel(), CancelWorkResult::CancelledPending);
    {
        let queue_state = test_pwq(queue.queue()).lock();
        assert_eq!(queue_state.in_flight_for_tests(flush_color), 0);
        assert_eq!(queue_state.in_flight_for_tests(later_color), 1);
        assert!(!queue_state.has_active_flush());
    }

    assert_eq!(second.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_dynamic_queue_color_flush_completion_is_generation_event() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-color-flush-event");
    let first = test_work(count_work);
    let second = test_work(count_work);

    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    let (first_color, first_flush_id, observed_generation) = {
        let mut queue_state = test_pwq(queue.queue()).lock();
        let color = queue_state.advance_work_color();
        let flush_id = queue_state
            .begin_flush(color)
            .expect("queued first work should start a color flush");
        (color, flush_id, queue.flush_event().generation())
    };

    assert_eq!(first.cancel(), CancelWorkResult::CancelledPending);
    assert!(queue.flush_event().has_changed_since(observed_generation));

    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    let second_flush_id = {
        let mut queue_state = test_pwq(queue.queue()).lock();
        assert!(!queue_state.is_flush_active(first_flush_id));
        let color = queue_state.advance_work_color();
        assert_ne!(color, first_color);
        queue_state
            .begin_flush(color)
            .expect("queued second work should start a new color flush")
    };

    {
        let queue_state = test_pwq(queue.queue()).lock();
        assert!(!queue_state.is_flush_active(first_flush_id));
        assert!(queue_state.is_flush_active(second_flush_id));
    }
    assert_eq!(second.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_work_color_wraps_after_linux_color_count() {
    let mut color = WorkColor::DEFAULT;
    for step in 0..WorkColor::COUNT {
        assert_eq!(color.index(), step);
        color = color.next();
    }
    assert_eq!(color, WorkColor::DEFAULT);
}

#[def_test(serial)]
fn test_queue_level_flush_reserves_distinct_snapshot_colors() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-queue-flush-colors")));
    let bindings = WorkQueuePoolBinding::for_all_owner_cpus(QueueOwner::Static(queue))
        .expect("test queue should resolve per-cpu bindings");
    let first_pwq = bindings
        .first()
        .expect("test queue should have at least one per-cpu binding");

    let first_color = {
        let mut binding = first_pwq.state().lock();
        let color = binding.work_color();
        binding.inc_in_flight(color);
        color
    };

    assert!(matches!(
        prepare_queue_color_flush(&bindings),
        QueueColorFlush::Wait(color) if color == first_color
    ));
    assert_eq!(first_pwq.state().lock().work_color(), first_color.next());

    let second_color = first_color.next();
    first_pwq.state().lock().inc_in_flight(second_color);

    assert!(matches!(
        prepare_queue_color_flush(&bindings),
        QueueColorFlush::Wait(color) if color == second_color
    ));
    assert_eq!(first_pwq.state().lock().work_color(), second_color.next());
    assert_eq!(
        first_pwq.state().lock().dec_in_flight(first_color),
        (false, true)
    );
    assert_eq!(
        first_pwq.state().lock().dec_in_flight(second_color),
        (false, true)
    );
}

#[def_test(serial)]
fn test_queue_level_flush_waits_when_next_color_is_in_flight() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-queue-flush-overflow")));
    let bindings = WorkQueuePoolBinding::for_all_owner_cpus(QueueOwner::Static(queue))
        .expect("test queue should resolve per-cpu bindings");
    let first_pwq = bindings
        .first()
        .expect("test queue should have at least one per-cpu binding");

    let (current_color, next_color) = {
        let mut binding = first_pwq.state().lock();
        let current_color = binding.work_color();
        let next_color = current_color.next();
        binding.inc_in_flight(next_color);
        (current_color, next_color)
    };

    assert!(matches!(
        prepare_queue_color_flush(&bindings),
        QueueColorFlush::Overflow(color) if color == next_color
    ));
    assert_eq!(first_pwq.state().lock().work_color(), current_color);

    assert_eq!(
        first_pwq.state().lock().dec_in_flight(next_color),
        (false, true)
    );
    assert!(matches!(
        prepare_queue_color_flush(&bindings),
        QueueColorFlush::Done
    ));
    assert_eq!(first_pwq.state().lock().work_color(), next_color);
}

#[def_test(serial)]
fn test_empty_static_queue_flush_completes() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-static-empty-flush")));

    assert_eq!(queue.flush(), Ok(()));
}

#[def_test(serial)]
fn test_static_queue_color_flush_completion_is_generation_event() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test-static-color-flush")));
    let work = test_work(count_work);

    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    let observed_generation = queue.sync().flush_event().generation();
    let flush_id = {
        let mut queue_state = test_pwq(queue).lock();
        let color = queue_state.advance_work_color();
        queue_state
            .begin_flush(color)
            .expect("queued static work should start a color flush")
    };

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    assert!(
        queue
            .sync()
            .flush_event()
            .has_changed_since(observed_generation)
    );
    assert!(!test_pwq(queue).lock().is_flush_active(flush_id));
}

#[def_test(serial)]
fn test_system_queue_color_flush_completion_is_generation_event() {
    let cpu_id = LogicalCpuId::new(0);
    let queue = system_wq();
    let work_binding = queue
        .select_pool_binding(Some(cpu_id))
        .expect("CPU 0 system queue should resolve");
    let system_binding = SystemPoolBinding::for_kind_cpu(SystemWorkQueueKind::Default, cpu_id)
        .expect("system queue should carry pool binding");
    let pool = system_binding.pool();
    let system_pwq = queue
        .pool_state_for_cpu(cpu_id)
        .expect("CPU 0 system queue should have a CPU 0 binding");
    let work = test_work(count_work);
    let observed_generation = queue.sync().flush_event().generation();
    let (color, flush_id) = {
        let mut pool_state = pool.state.lock();
        let mut binding_state = system_pwq.lock();
        let mut work_state = work.inner().state.lock();
        let color = binding_state.work_color();
        let instance_id = work_state.allocate_instance_id();
        let was_idle = binding_state.is_idle();
        pool_state
            .pending
            .push(&work, work_binding.owner(), color)
            .expect("test system pool should accept pending work");
        work.inner().done.reinit();
        work_state.set_pending(instance_id, work_binding.clone(), color);
        binding_state.inc_in_flight(color);
        if was_idle {
            queue.sync().idle_completion().reinit();
        }
        let flush_id = binding_state
            .begin_flush(color)
            .expect("queued system work should start a color flush");
        (color, flush_id)
    };

    assert_eq!(system_pwq.lock().in_flight_for_tests(color), 1);
    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    {
        let binding_state = system_pwq.lock();
        assert_eq!(binding_state.in_flight_for_tests(color), 0);
        assert!(!binding_state.is_flush_active(flush_id));
    }
    assert!(
        queue
            .sync()
            .flush_event()
            .has_changed_since(observed_generation)
    );
}

#[def_test(serial)]
fn test_delayed_conversion_counts_only_committed_dynamic_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-color-delayed");
    let work = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&work, DelayedWorkTarget::Dynamic(queue.clone()));
    let color = test_pwq(queue.queue()).lock().work_color();

    assert_eq!(
        DelayedWorkTarget::Dynamic(queue.clone()).queue_reserved_work(work.scheduled(), seq),
        QueueWorkResult::Queued
    );

    assert_eq!(test_pwq(queue.queue()).lock().in_flight_for_tests(color), 1);
    assert_eq!(
        work.scheduled().cancel(),
        CancelWorkResult::CancelledPending
    );
    assert_eq!(test_pwq(queue.queue()).lock().in_flight_for_tests(color), 0);
}

#[def_test(serial)]
fn test_dynamic_flush_waits_for_inactive_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = WorkQueueHandle::new("test-color-flush-inactive");
    queue.configure_max_active_for_tests(1);
    let active = test_work(count_work);
    let inactive = test_work(count_work);

    assert_eq!(queue.queue_work(&active), QueueWorkResult::Queued);
    let flush_color = test_pwq(queue.queue()).lock().work_color();
    assert_eq!(queue.queue_work(&inactive), QueueWorkResult::Queued);
    let flush_id = {
        let mut queue_state = test_pwq(queue.queue()).lock();
        assert_eq!(queue_state.in_flight_for_tests(flush_color), 2);
        queue_state
            .begin_flush(flush_color)
            .expect("active and inactive work should keep color flush pending")
    };

    assert_eq!(active.cancel(), CancelWorkResult::CancelledPending);
    {
        let queue_state = test_pwq(queue.queue()).lock();
        assert_eq!(queue_state.in_flight_for_tests(flush_color), 1);
        assert!(queue_state.is_flush_active(flush_id));
    }
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);

    assert_eq!(inactive.cancel(), CancelWorkResult::CancelledPending);
    {
        let queue_state = test_pwq(queue.queue()).lock();
        assert_eq!(queue_state.in_flight_for_tests(flush_color), 0);
        assert!(!queue_state.is_flush_active(flush_id));
    }
}

#[def_test(serial)]
fn test_dynamic_workqueue_flush_rejects_same_pool_callback() {
    let queue = WorkQueueHandle::new("test-dynamic");
    let outer = flush_dynamic_queue_from_callback(queue.clone());
    let inner = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    DYNAMIC_QUEUE_WAIT_RESULT.store(0, Ordering::Release);
    assert_eq!(queue.queue_work(&outer), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&inner), QueueWorkResult::Queued);

    outer.flush().expect("outer callback should finish");
    assert_eq!(DYNAMIC_QUEUE_WAIT_RESULT.load(Ordering::Acquire), 2);
    inner.flush().expect("inner work should finish");
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_queue_work_publishes_prior_store_to_callback() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(observe_store_work);

    STORE_BEFORE_QUEUE.store(42, Ordering::Release);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_OBSERVED.load(Ordering::Acquire), 42);
}

#[def_test(serial)]
fn test_queue_work_reports_queue_full_for_idle_work() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut works = Vec::new();

    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        works.push(work);
    }

    let overflow = test_work(count_work);
    assert_eq!(queue.queue_work(&overflow), QueueWorkResult::QueueFull);
    assert_eq!(queue.pending_len_for_tests(), MAX_WORKQUEUE_PENDING);
    frozen_pool.release_and_flush();
    queue.flush().expect("full queue should drain");
}

#[def_test(serial)]
fn test_queue_work_rejects_running_requeue() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut works = Vec::new();

    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        works.push(work);
    }

    let running = test_work(count_work);
    mark_running_for_tests(&running, queue);
    assert_eq!(queue.queue_work(&running), QueueWorkResult::AlreadyQueued);
    assert_eq!(running.inner().state.lock().status(), WorkStatus::Running);
    finish_work(queue, &running);
    frozen_pool.release_and_flush();
    queue.flush().expect("full queue should drain");
}

#[def_test(serial)]
fn test_workqueue_ring_buffer_preserves_fifo_after_wrap() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut works = Vec::new();

    WORK_OBSERVED.store(0, Ordering::Relaxed);
    STORE_BEFORE_QUEUE.store(0, Ordering::Release);

    for expected in 0..MAX_WORKQUEUE_PENDING {
        let work = ScheduledWork::new(move |_work| {
            let observed = WORK_OBSERVED.fetch_add(1, Ordering::Relaxed);
            if observed != expected {
                STORE_BEFORE_QUEUE.store(expected + 1, Ordering::Release);
            }
        });
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        works.push(work);
    }

    for _ in 0..(MAX_WORKQUEUE_PENDING / 2) {
        assert!(drain_one_test_work(WorkerId::new(0)));
    }

    for expected in MAX_WORKQUEUE_PENDING..(MAX_WORKQUEUE_PENDING + MAX_WORKQUEUE_PENDING / 2) {
        let work = ScheduledWork::new(move |_work| {
            let observed = WORK_OBSERVED.fetch_add(1, Ordering::Relaxed);
            if observed != expected {
                STORE_BEFORE_QUEUE.store(expected + 1, Ordering::Release);
            }
        });
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        works.push(work);
    }

    while drain_one_test_work(WorkerId::new(0)) {}

    assert_eq!(
        WORK_OBSERVED.load(Ordering::Relaxed),
        MAX_WORKQUEUE_PENDING + MAX_WORKQUEUE_PENDING / 2
    );
    assert_eq!(STORE_BEFORE_QUEUE.load(Ordering::Acquire), 0);
}

#[def_test(serial)]
fn test_cancel_work_removes_wrapped_pending_entry() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let mut works = Vec::new();

    WORK_RUNS.store(0, Ordering::Relaxed);
    for _ in 0..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        works.push(work);
    }

    for _ in 0..(MAX_WORKQUEUE_PENDING / 2) {
        assert!(drain_one_test_work(WorkerId::new(0)));
    }

    let wrapped = test_work(count_work);
    assert_eq!(queue.queue_work(&wrapped), QueueWorkResult::Queued);
    assert_eq!(wrapped.cancel(), CancelWorkResult::CancelledPending);

    while drain_one_test_work(WorkerId::new(0)) {}
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), MAX_WORKQUEUE_PENDING);
}

#[def_test(serial)]
fn test_queue_work_rejects_canceling_work() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    mark_canceling_for_tests(&work, queue);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Disabled);
    assert_eq!(queue.pending_len_for_tests(), 0);
    finish_work(queue, &work);
}

#[def_test(serial)]
fn test_delayed_work_rejects_canceling_work() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let delayed = DelayedScheduledWork::new(count_work);

    mark_canceling_for_tests(delayed.scheduled(), queue);
    assert_eq!(
        queue.queue_delayed_work(&delayed, TimeSpan::from_secs(60)),
        QueueDelayedWorkResult::Disabled
    );
    finish_work(queue, delayed.scheduled());
}

#[def_test(serial)]
fn test_non_waiting_cancel_removes_pending_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert!(queue.has_runnable_work_for_tests());
    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    assert!(!queue.has_runnable_work_for_tests());
    assert!(!drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn test_start_workqueue_rejects_invalid_static_inputs() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let system_queue = system_wq();

    assert_eq!(
        system_queue.start(WorkQueueAttrs::new()),
        Err(WorkQueueStartError::SystemQueue)
    );
    assert_eq!(
        queue.start(WorkQueueAttrs::new().with_flags(WorkQueueFlags::UNBOUND)),
        Err(WorkQueueStartError::UnsupportedFlags)
    );
    assert_eq!(
        queue.start(WorkQueueAttrs::new().with_flags(WorkQueueFlags::BH)),
        Err(WorkQueueStartError::UnsupportedFlags)
    );
}

#[def_test(serial)]
fn test_alloc_workqueue_rejects_unsupported_linux_attrs() {
    assert!(matches!(
        WorkQueueHandle::alloc(
            "test-unsupported-flags",
            WorkQueueAttrs::new().with_flags(WorkQueueFlags::MEM_RECLAIM)
        ),
        Err(WorkQueueAllocError::UnsupportedFlags)
    ));
    assert!(matches!(
        WorkQueueHandle::alloc(
            "test-unsupported-bh",
            WorkQueueAttrs::new().with_flags(WorkQueueFlags::BH)
        ),
        Err(WorkQueueAllocError::UnsupportedFlags)
    ));
}

#[def_test(serial)]
fn test_dynamic_queue_max_active_throttles_until_finish() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(1);
    let first = test_work(count_work);
    let second = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    assert_eq!(queue.pending_len_for_tests(), 2);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(queue.pending_len_for_tests(), 1);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
    assert_eq!(queue.pending_len_for_tests(), 0);
    assert_eq!(queue.runnable_len_for_tests(), 0);
    assert_eq!(queue.active_len_for_tests(), 0);
}

#[def_test(serial)]
fn test_dynamic_queue_max_active_two_allows_two_runnable() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(2);
    let first = test_work(count_work);
    let second = test_work(count_work);

    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    assert_eq!(queue.pending_len_for_tests(), 2);
    assert_eq!(queue.runnable_len_for_tests(), 2);
    assert_eq!(queue.active_len_for_tests(), 2);
    frozen_pool.release_and_flush();
    queue.flush().expect("remaining work should drain");
}

#[def_test(serial)]
fn test_configuring_max_active_reclassifies_existing_entries() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let first = test_work(count_work);
    let second = test_work(count_work);

    assert_eq!(queue.queue_work(&first), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&second), QueueWorkResult::Queued);
    assert_eq!(queue.runnable_len_for_tests(), 2);
    assert_eq!(queue.active_len_for_tests(), 2);

    queue.configure_max_active_for_tests(1);
    assert_eq!(queue.pending_len_for_tests(), 2);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(queue.pending_len_for_tests(), 1);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);
    frozen_pool.release_and_flush();
    queue.flush().expect("remaining work should drain");
}

#[def_test(serial)]
fn test_cancel_inactive_work_keeps_active_token() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(1);
    let active = test_work(count_work);
    let inactive = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&active), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&inactive), QueueWorkResult::Queued);
    assert_eq!(inactive.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(queue.pending_len_for_tests(), 1);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_delayed_conversion_enters_inactive_when_max_active_full() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(1);
    let active = test_work(count_work);
    let delayed = DelayedScheduledWork::new(count_work);
    let seq = arm_delayed_for_tests(&delayed, DelayedWorkTarget::Static(queue));

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&active), QueueWorkResult::Queued);
    assert_eq!(
        DelayedWorkTarget::Static(queue).queue_reserved_work(delayed.scheduled(), seq),
        QueueWorkResult::Queued
    );
    assert_eq!(queue.pending_len_for_tests(), 2);
    assert_eq!(queue.runnable_len_for_tests(), 1);
    assert_eq!(queue.active_len_for_tests(), 1);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn test_flush_work_reports_idle_work() {
    let work = test_work(count_work);

    assert_eq!(work.flush(), Ok(false));
}

#[def_test(serial)]
fn test_pending_flush_barrier_is_attached_to_queue_entry() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    let color = work.inner().state.lock().pending_color();
    let in_flight_before_barrier = test_pwq(queue).lock().in_flight_for_tests(color);
    let barrier = attach_flush_barrier(&work, None)
        .expect("pending flush barrier attach should not fail")
        .expect("pending work should accept a flush barrier");
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier + 1
    );
    assert!(!barrier.completion().try_wait());

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier - 1
    );
    assert!(barrier.completion().try_wait());
}

#[def_test(serial)]
fn test_linked_barrier_front_insert_matches_linux_position_order() {
    let mut queue = WorkBarrierQueue::new();
    let first = WorkBarrier::new();
    let second = WorkBarrier::new();
    let third = WorkBarrier::new();

    assert!(queue.push_front_linked_barrier(first.clone()));
    assert!(queue.push_front_linked_barrier(second.clone()));
    assert!(queue.push_front_linked_barrier(third.clone()));

    let barriers = queue.take_all();
    assert_eq!(barriers.len(), 3);
    assert!(barriers[0].same_barrier(&third));
    assert!(barriers[1].same_barrier(&second));
    assert!(barriers[2].same_barrier(&first));
}

#[def_test(serial)]
fn test_running_barrier_insert_precedes_transferred_pending_chain() {
    let mut queue = WorkBarrierQueue::new();
    let pending_first = WorkBarrier::new();
    let pending_second = WorkBarrier::new();
    let running_barrier = WorkBarrier::new();
    let pending_chain = [pending_second.clone(), pending_first.clone()]
        .into_iter()
        .collect();
    assert!(queue.append_from_vec(pending_chain));
    assert!(queue.push_front_linked_barrier(running_barrier.clone()));

    let barriers = queue.take_all();
    assert_eq!(barriers.len(), 3);
    assert!(barriers[0].same_barrier(&running_barrier));
    assert!(barriers[1].same_barrier(&pending_second));
    assert!(barriers[2].same_barrier(&pending_first));
}

#[def_test(serial)]
fn test_pending_flush_barrier_does_not_consume_queue_capacity() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let target = test_work(count_work);
    let mut queued = Vec::new();

    assert_eq!(queue.queue_work(&target), QueueWorkResult::Queued);
    let color = target.inner().state.lock().pending_color();
    for _ in 1..MAX_WORKQUEUE_PENDING {
        let work = test_work(count_work);
        assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
        queued.push(work);
    }
    assert_eq!(queue.pending_len_for_tests(), MAX_WORKQUEUE_PENDING);

    let in_flight_before_barrier = test_pwq(queue).lock().in_flight_for_tests(color);
    let barrier = attach_flush_barrier(&target, None)
        .expect("pending flush barrier attach should not fail")
        .expect("pending work should accept a flush barrier");
    assert_eq!(queue.pending_len_for_tests(), MAX_WORKQUEUE_PENDING);
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier + 1
    );
    assert!(!barrier.completion().try_wait());

    assert_eq!(target.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier - 1
    );
    assert!(barrier.completion().try_wait());
    frozen_pool.release_and_flush();
    queue.flush().expect("remaining work should drain");
}

#[def_test(serial)]
fn test_pending_flush_barrier_reports_full_storage() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);
    let mut barriers = Vec::new();

    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    let color = work.inner().state.lock().pending_color();
    let in_flight_before_barriers = test_pwq(queue).lock().in_flight_for_tests(color);
    for _ in 0..MAX_WORK_BARRIERS_PER_SLOT {
        let barrier = attach_flush_barrier(&work, None)
            .expect("pending flush barrier attach should not fail before slot is full")
            .expect("pending work should accept a flush barrier");
        barriers.push(barrier);
    }
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers + MAX_WORK_BARRIERS_PER_SLOT
    );

    assert_eq!(work.flush(), Err(WorkqueueError::BarrierFull));
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers + MAX_WORK_BARRIERS_PER_SLOT
    );

    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers - 1
    );
    assert!(
        barriers
            .iter()
            .all(|barrier| barrier.completion().try_wait())
    );
}

#[def_test(serial)]
fn test_running_flush_barrier_attaches_to_pool_worker_slot() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    mark_running_for_tests(&work, queue);
    let color = work.inner().state.lock().running_color();
    let in_flight_before_barrier = test_pwq(queue).lock().in_flight_for_tests(color);

    let barrier = attach_flush_barrier(&work, None)
        .expect("running flush barrier attach should not fail")
        .expect("running work should accept a flush barrier");
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier + 1
    );
    assert!(!barrier.completion().try_wait());

    finish_work(queue, &work);
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barrier - 1
    );
    assert!(barrier.completion().try_wait());
}

#[def_test(serial)]
fn test_running_flush_barrier_reports_full_storage() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);
    let mut barriers = Vec::new();

    mark_running_for_tests(&work, queue);
    let color = work.inner().state.lock().running_color();
    let in_flight_before_barriers = test_pwq(queue).lock().in_flight_for_tests(color);
    for _ in 0..MAX_WORK_BARRIERS_PER_SLOT {
        let barrier = attach_flush_barrier(&work, None)
            .expect("running flush barrier attach should not fail before slot is full")
            .expect("running work should accept a flush barrier");
        barriers.push(barrier);
    }
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers + MAX_WORK_BARRIERS_PER_SLOT
    );

    assert_eq!(work.flush(), Err(WorkqueueError::BarrierFull));
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers + MAX_WORK_BARRIERS_PER_SLOT
    );

    finish_work(queue, &work);
    assert_eq!(
        test_pwq(queue).lock().in_flight_for_tests(color),
        in_flight_before_barriers - 1
    );
    assert!(
        barriers
            .iter()
            .all(|barrier| barrier.completion().try_wait())
    );
}

#[def_test(serial)]
fn test_flush_work_rejects_self_wait_from_explicit_worker() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(self_flush_work);

    SELF_WAIT_RESULT.store(0, Ordering::Release);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    work.flush().expect("self-wait callback should finish");
    assert_eq!(SELF_WAIT_RESULT.load(Ordering::Acquire), 2);
}

#[def_test(serial)]
fn test_pool_drain_rejects_nested_callback_execution() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(1);
    let outer = test_work(nested_run_work);
    let inner = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    NESTED_RUN_RESULT.store(usize::MAX, Ordering::Release);
    assert_eq!(queue.queue_work(&outer), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&inner), QueueWorkResult::Queued);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(NESTED_RUN_RESULT.load(Ordering::Acquire), 0);
    frozen_pool.release_and_flush();
    inner
        .flush()
        .expect("inner work should finish after outer work");
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    assert_eq!(queue.pending_len_for_tests(), 0);
}

#[def_test(serial)]
fn test_flush_work_rejects_same_queue_pending_wait_from_callback() {
    let frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    queue.configure_max_active_for_tests(1);
    let outer = test_work(flush_other_pending_work);
    let inner = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    SAME_QUEUE_WAIT_RESULT.store(0, Ordering::Release);
    let _target_guard = SameQueueWaitTargetGuard::install(inner.clone());

    assert_eq!(queue.queue_work(&outer), QueueWorkResult::Queued);
    assert_eq!(queue.queue_work(&inner), QueueWorkResult::Queued);
    frozen_pool.release_and_flush();
    outer.flush().expect("outer callback should finish");
    assert_eq!(SAME_QUEUE_WAIT_RESULT.load(Ordering::Acquire), 2);
    inner.flush().expect("inner work should finish");
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn test_flush_work_rejects_running_work_from_worker_callback() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let outer = test_work(count_work);
    let target = test_work(count_work);

    {
        mark_running_for_tests(&target, queue);
    }
    let _current = CurrentWorkGuard::enter(
        queue,
        test_pool_binding().pool().key(),
        WorkerId::new(0),
        WorkerExecutionToken::from_usize(1),
        &outer,
    );

    assert_eq!(target.flush(), Err(WorkqueueError::SelfWait));
    drop(_current);
    finish_work(queue, &target);
}

#[def_test(serial)]
fn test_running_queue_attempt_does_not_change_waiter_state() {
    let running_queue = Box::leak(Box::new(WorkQueue::new("running")));
    let other_queue = Box::leak(Box::new(WorkQueue::new("other")));
    let target = test_work(count_work);

    mark_running_for_tests(&target, running_queue);
    let observed_generation = target.inner().state_change.generation();

    assert_eq!(
        other_queue.queue_work(&target),
        QueueWorkResult::AlreadyQueued
    );
    assert_eq!(target.inner().state.lock().status(), WorkStatus::Running);
    assert!(
        !target
            .inner()
            .state_change
            .has_changed_since(observed_generation)
    );
    finish_work(running_queue, &target);
}

#[def_test(serial)]
fn test_idle_enqueue_marks_state_change_for_waiter_recheck() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    let observed_generation = work.inner().state_change.generation();
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);

    assert!(!work.inner().done.try_wait());
    assert!(
        work.inner()
            .state_change
            .has_changed_since(observed_generation)
    );
    assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
}

#[def_test(serial)]
fn test_running_queue_attempt_does_not_change_state_for_waiter_recheck() {
    let running_queue = Box::leak(Box::new(WorkQueue::new("running")));
    let other_queue = Box::leak(Box::new(WorkQueue::new("other")));
    let work = test_work(count_work);

    mark_running_for_tests(&work, running_queue);
    let observed_generation = work.inner().state_change.generation();
    assert_eq!(
        other_queue.queue_work(&work),
        QueueWorkResult::AlreadyQueued
    );

    assert!(
        !work
            .inner()
            .state_change
            .has_changed_since(observed_generation)
    );
    finish_work(running_queue, &work);
}

#[def_test(serial)]
fn test_cancel_work_sync_removes_pending_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);
    assert_eq!(work.cancel_sync(), Ok(true));
    assert!(!drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn test_running_work_queue_attempt_is_rejected() {
    let queue = Box::leak(Box::new(WorkQueue::new("test")));
    let work = test_work(count_work);

    mark_running_for_tests(&work, queue);

    assert_eq!(queue.queue_work(&work), QueueWorkResult::AlreadyQueued);
    assert_eq!(queue.pending_len_for_tests(), 0);
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Running);
    finish_work(queue, &work);
}

#[def_test(serial)]
fn test_running_work_queue_attempt_is_rejected_across_queues() {
    let running_queue = Box::leak(Box::new(WorkQueue::new("running")));
    let other_queue = Box::leak(Box::new(WorkQueue::new("other")));
    let work = test_work(count_work);

    mark_running_for_tests(&work, running_queue);

    assert_eq!(
        other_queue.queue_work(&work),
        QueueWorkResult::AlreadyQueued
    );
    assert_eq!(running_queue.pending_len_for_tests(), 0);
    assert_eq!(other_queue.pending_len_for_tests(), 0);
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Running);
    finish_work(running_queue, &work);
}

#[def_test(serial)]
fn test_system_pool_runnable_query_ignores_running_work() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-system")));
    let pool = Box::leak(Box::new(WorkerPool::new()));
    let worker_id = WorkerId::new(0);
    let work = test_work(count_work);

    {
        let mut pool_state = pool.state.lock();
        pool_state.workers[worker_id.as_usize()].state = WorkerState::Idle;
        pool_state.workers[worker_id.as_usize()].current_work_key = 0;
        assert_eq!(pool_state.pending.len(), 0);
        assert_eq!(pool_state.runnable_count, 0);
        assert_eq!(
            pool_state.workers[worker_id.as_usize()].state,
            WorkerState::Idle
        );
        let owner = QueueOwner::Static(queue);
        let binding = WorkQueuePoolBinding::for_test_pool(owner.clone(), pool);
        pool_state
            .pending
            .push(&work, owner, WorkColor::DEFAULT)
            .expect("test system pool should have capacity");
        pool_state.runnable_count += 1;
        let mut work_state = work.inner().state.lock();
        let seq = work_state.allocate_instance_id();
        work_state.set_pending(seq, binding, WorkColor::DEFAULT);
    }

    let outcome = pool
        .state
        .lock()
        .take_any_runnable_work(pool.key(), worker_id);
    let worker_token = outcome
        .worker_token
        .expect("running test work should carry a worker execution token");
    let running = outcome
        .work
        .expect("test system work should become running");
    assert!(running.same_work(&work));
    assert!(!pool.state.lock().has_runnable_work());
    assert_eq!(queue.queue_work(&work), QueueWorkResult::AlreadyQueued);
    let instance_id = work
        .inner()
        .state
        .lock()
        .running_instance_id()
        .expect("test work should still carry its running instance");
    work.inner().state.lock().set_idle();
    let _ = pool.state.lock().finish_running_work(
        worker_id.as_usize(),
        work.key(),
        instance_id,
        worker_token,
    );
}

#[def_test(serial)]
fn test_stale_pool_entry_does_not_decrement_running_count() {
    let queue = Box::leak(Box::new(WorkQueue::new("stale-accounting")));
    let pool = Box::leak(Box::new(WorkerPool::new()));
    let worker_id = WorkerId::new(0);
    let stale_work = test_work(count_work);
    let valid_work = test_work(count_work);
    let owner = QueueOwner::Static(queue);
    let mut binding = WorkQueuePoolState::new();

    binding.configure_max_active(3);
    binding.add_active();
    binding.start_running();
    binding.add_active();
    binding.add_active();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(worker_id.as_usize()));
    pool_state
        .pending
        .push(&stale_work, owner.clone(), WorkColor::DEFAULT)
        .expect("test pool should have capacity");
    pool_state
        .pending
        .push(&valid_work, owner.clone(), WorkColor::DEFAULT)
        .expect("test pool should have capacity");
    pool_state.runnable_count = 2;
    let work_binding = WorkQueuePoolBinding::for_test_pool(owner, pool);
    mark_pending_for_tests(&valid_work, work_binding);

    let outcome = pool_state.take_any_runnable_work(pool.key(), worker_id);
    let worker_token = outcome
        .worker_token
        .expect("valid work should carry a worker execution token");
    assert!(outcome.work.is_some_and(|work| work.same_work(&valid_work)));
    assert_eq!(outcome.stale_entries.len(), 1);
    pool_state.discard_active_entry_locked(&mut binding, queue.key());

    assert!(binding.has_running());
    assert_eq!(binding.active_count_for_tests(), 2);
    assert_eq!(pool_state.runnable_count, 0);

    let instance_id = valid_work
        .inner()
        .state
        .lock()
        .running_instance_id()
        .expect("valid work should carry its running instance");
    valid_work.inner().state.lock().set_idle();
    let _ = pool_state.finish_running_work(
        worker_id.as_usize(),
        valid_work.key(),
        instance_id,
        worker_token,
    );
}

#[def_test(serial)]
fn test_pool_entry_claim_rejects_stale_instance() {
    let queue = Box::leak(Box::new(WorkQueue::new("stale-instance")));
    let pool = Box::leak(Box::new(WorkerPool::new()));
    let worker_id = WorkerId::new(0);
    let work = test_work(count_work);
    let owner = QueueOwner::Static(queue);

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(worker_id.as_usize()));
    pool_state
        .pending
        .push_runnable(WorkEntry::new(
            work.clone(),
            owner.clone(),
            WorkColor::DEFAULT,
            WorkInstanceId::for_tests(0xBADC0DE),
        ))
        .expect("test pool should have capacity");
    pool_state.runnable_count = 1;
    let work_binding = WorkQueuePoolBinding::for_test_pool(owner, pool);
    mark_pending_for_tests(&work, work_binding);

    let outcome = pool_state.take_any_runnable_work(pool.key(), worker_id);

    assert!(outcome.work.is_none());
    assert_eq!(outcome.stale_entries.len(), 1);
    assert_eq!(pool_state.runnable_count, 0);
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Pending);
}

#[def_test(serial)]
fn test_system_pool_selects_one_preparing_worker() {
    let pool = WorkerPool::new();

    {
        let mut pool_state = pool.state.lock();
        assert!(pool_state.install_worker(0));
        assert!(pool_state.install_worker(1));
        pool_state.runnable_count = 1;

        let plan = pool_state.select_worker_to_kick();
        assert_eq!(plan.worker_to_wake, Some(WorkerId::new(0)));
        assert!(!plan.should_wake_manager);
        assert_eq!(pool_state.workers[0].state, WorkerState::Preparing);
        let plan = pool_state.select_worker_to_kick();
        assert_eq!(plan.worker_to_wake, None);
        assert!(!plan.should_wake_manager);
        assert_eq!(pool_state.workers[1].state, WorkerState::Idle);
        assert_eq!(pool_state.nr_running, 0);
        assert!(!pool_state.manager_needed);
    }
}

#[def_test(serial)]
fn test_system_pool_sleeping_worker_kicks_idle_worker() {
    let pool = WorkerPool::new();

    {
        let mut pool_state = pool.state.lock();
        assert!(pool_state.install_worker(0));
        assert!(pool_state.install_worker(1));
        pool_state.start_running_work(0, 0x51, WorkInstanceId::for_tests(0x51), Vec::new());
        pool_state.runnable_count = 1;

        let transition = pool_state.mark_worker_sleeping(0);
        assert!(transition.did_sleep);
        assert_eq!(transition.wake_plan.worker_to_wake, Some(WorkerId::new(1)));
        assert!(!transition.wake_plan.should_wake_manager);
        assert_eq!(pool_state.workers[0].state, WorkerState::Sleeping);
        assert_eq!(pool_state.workers[0].current_work_key, 0x51);
        assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);
        assert_eq!(pool_state.nr_running, 0);
    }
}

#[def_test(serial)]
fn test_system_pool_prepare_wait_clears_stale_preparing_worker() {
    let pool = WorkerPool::new();

    {
        let mut pool_state = pool.state.lock();
        assert!(pool_state.install_worker(0));
        assert!(pool_state.install_worker(1));
        pool_state.start_running_work(0, 0x84, WorkInstanceId::for_tests(0x84), Vec::new());
        pool_state.runnable_count = 1;

        let transition = pool_state.mark_worker_sleeping(0);
        assert!(transition.did_sleep);
        assert_eq!(transition.wake_plan.worker_to_wake, Some(WorkerId::new(1)));
        assert!(!transition.wake_plan.should_wake_manager);
        assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);

        pool_state.runnable_count = 0;
        assert!(!pool_state.prepare_worker_to_wait(1));
        assert_eq!(pool_state.workers[1].state, WorkerState::Idle);

        pool_state.runnable_count = 1;
        let plan = pool_state.select_worker_to_kick();
        assert_eq!(plan.worker_to_wake, Some(WorkerId::new(1)));
        assert!(!plan.should_wake_manager);
        assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);
    }
}

#[def_test(serial)]
fn test_system_pool_sleep_resume_restores_running_count() {
    let pool = WorkerPool::new();

    {
        let mut pool_state = pool.state.lock();
        assert!(pool_state.install_worker(0));
        pool_state.start_running_work(0, 0x62, WorkInstanceId::for_tests(0x62), Vec::new());
        assert_eq!(pool_state.nr_running, 1);

        let transition = pool_state.mark_worker_sleeping(0);
        assert!(transition.did_sleep);
        assert_eq!(transition.wake_plan.worker_to_wake, None);
        assert!(!transition.wake_plan.should_wake_manager);
        assert!(!pool_state.manager_needed);
        assert_eq!(pool_state.workers[0].state, WorkerState::Sleeping);
        assert_eq!(pool_state.nr_running, 0);

        let transition = pool_state.mark_worker_sleeping(0);
        assert!(!transition.did_sleep);
        assert_eq!(transition.wake_plan.worker_to_wake, None);
        assert!(!transition.wake_plan.should_wake_manager);
        assert_eq!(pool_state.workers[0].state, WorkerState::Sleeping);
        assert_eq!(pool_state.nr_running, 0);

        pool_state.mark_worker_running(0);
        assert_eq!(pool_state.workers[0].state, WorkerState::Running);
        assert_eq!(pool_state.workers[0].current_work_key, 0x62);
        assert_eq!(pool_state.nr_running, 1);
    }
}

#[def_test(serial)]
fn test_system_pool_finish_sleeping_worker_does_not_double_decrement() {
    let pool = WorkerPool::new();

    {
        let mut pool_state = pool.state.lock();
        assert!(pool_state.install_worker(0));
        let worker_token =
            pool_state.start_running_work(0, 0x73, WorkInstanceId::for_tests(0x73), Vec::new());
        let transition = pool_state.mark_worker_sleeping(0);
        assert!(transition.did_sleep);
        assert_eq!(transition.wake_plan.worker_to_wake, None);

        let _ =
            pool_state.finish_running_work(0, 0x73, WorkInstanceId::for_tests(0x73), worker_token);
        assert_eq!(pool_state.workers[0].state, WorkerState::Idle);
        assert_eq!(pool_state.workers[0].current_work_key, 0);
        assert_eq!(pool_state.nr_running, 0);
    }
}

#[def_test(serial)]
fn test_system_pool_manager_reserves_empty_worker_slot() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.start_running_work(0, 0x90, WorkInstanceId::for_tests(0x90), Vec::new());
    pool_state.runnable_count = 1;

    let transition = pool_state.mark_worker_sleeping(0);
    assert!(transition.wake_plan.should_wake_manager);
    assert_eq!(pool_state.reserve_worker_creation(), Some(1));
    assert_eq!(pool_state.workers[1].state, WorkerState::Creating);
    assert!(pool_state.reserve_worker_creation().is_none());

    assert!(pool_state.install_worker(1));
    let plan = pool_state.finish_worker_creation(1, true);
    assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);
    assert_eq!(plan.worker_to_wake, Some(WorkerId::new(1)));
    assert!(!plan.should_wake_manager);
}

#[def_test(serial)]
fn test_system_pool_manager_failure_enters_retry_cooldown() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.start_running_work(0, 0x91, WorkInstanceId::for_tests(0x91), Vec::new());
    pool_state.runnable_count = 1;

    let transition = pool_state.mark_worker_sleeping(0);
    assert!(transition.wake_plan.should_wake_manager);
    assert_eq!(pool_state.reserve_worker_creation(), Some(1));

    let plan = pool_state.finish_worker_creation(1, false);
    assert_eq!(pool_state.workers[1].state, WorkerState::Empty);
    assert!(pool_state.manager_needed);
    assert_eq!(plan, WorkerWakePlan::default());
    assert_eq!(pool_state.reserve_worker_creation(), None);
    assert!(!pool_state.worker_creation_retry_ready());

    pool_state.set_worker_create_retry_delay_for_tests(TimeSpan::ZERO);
    let plan = pool_state.select_worker_to_kick();
    assert!(plan.should_wake_manager);
    assert_eq!(pool_state.reserve_worker_creation(), Some(1));
}

#[def_test(serial)]
fn test_system_pool_manager_rechecks_need_before_reserving() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.start_running_work(0, 0x92, WorkInstanceId::for_tests(0x92), Vec::new());
    pool_state.runnable_count = 1;

    let transition = pool_state.mark_worker_sleeping(0);
    assert!(transition.wake_plan.should_wake_manager);
    assert!(pool_state.manager_needed);

    pool_state.runnable_count = 0;
    assert_eq!(pool_state.reserve_worker_creation(), None);
    assert!(!pool_state.manager_needed);
    assert_eq!(pool_state.workers[1].state, WorkerState::Empty);
}

#[def_test(serial)]
fn test_destroying_dynamic_workqueue_waits_for_running_work() {
    let queue = WorkQueueHandle::new("test-dynamic");
    let work = test_work(count_work);
    let rejected = test_work(count_work);
    mark_running_owner_for_tests(&work, QueueOwner::Dynamic(queue.clone()));

    queue.queue().state.lock().is_destroying = true;
    assert!(!queue.queue().is_idle());
    assert_eq!(queue.queue_work(&rejected), QueueWorkResult::Disabled);

    finish_work(queue.queue(), &work);
    assert!(queue.queue().is_idle());
}

#[def_test(serial)]
fn test_system_worker_slot_must_be_idle_to_drain() {
    let queue = Box::leak(Box::new(WorkQueue::new("test-system")));
    let pool = Box::leak(Box::new(WorkerPool::new()));
    let worker_id = WorkerId::new(0);
    let work = test_work(count_work);

    {
        let mut pool_state = pool.state.lock();
        let owner = QueueOwner::Static(queue);
        let binding = WorkQueuePoolBinding::for_test_pool(owner.clone(), pool);
        {
            let mut work_state = work.inner().state.lock();
            let seq = work_state.allocate_instance_id();
            work_state.set_pending(seq, binding.clone(), WorkColor::DEFAULT);
        }
        pool_state
            .pending
            .push(&work, owner, WorkColor::DEFAULT)
            .expect("test pool has capacity");
        pool_state.runnable_count = 1;
        pool_state.workers[worker_id.as_usize()].state = WorkerState::Running;
    }

    assert!(
        pool.state
            .lock()
            .take_any_runnable_work(pool.key(), worker_id)
            .work
            .is_none()
    );
    assert_eq!(pool.state.lock().runnable_count, 1);
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Pending);

    pool.state.lock().workers[worker_id.as_usize()].state = WorkerState::Idle;
    let outcome = pool
        .state
        .lock()
        .take_any_runnable_work(pool.key(), worker_id);
    let worker_token = outcome
        .worker_token
        .expect("running test work should carry a worker execution token");
    let taken = outcome
        .work
        .expect("idle worker slot should be allowed to drain");
    assert!(taken.same_work(&work));
    let instance_id = work
        .inner()
        .state
        .lock()
        .running_instance_id()
        .expect("test work should still carry its running instance");
    work.inner().state.lock().set_idle();
    let _ = pool.state.lock().finish_running_work(
        worker_id.as_usize(),
        work.key(),
        instance_id,
        worker_token,
    );
}

#[def_test(serial)]
fn test_schedule_work_on_rejects_out_of_range_cpu() {
    let invalid_cpu = LogicalCpuId::new(kbuild_config::NR_CPUS);
    let work = test_work(count_work);

    assert_eq!(
        schedule_work_on(invalid_cpu, &work),
        QueueWorkResult::InvalidCpu
    );
    assert_eq!(work.inner().state.lock().status(), WorkStatus::Idle);
}

#[def_test(serial)]
fn test_system_percpu_wq_aliases_default_system_instance() {
    let percpu_queue = system_percpu_wq();
    let compat_queue = system_wq();

    assert!(core::ptr::eq(system_percpu_wq(), percpu_queue));
    assert!(core::ptr::eq(percpu_queue, compat_queue));
}

#[def_test(serial)]
fn test_system_bh_wq_is_global_with_per_cpu_pools() {
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    let queue = system_bh_wq();

    assert!(core::ptr::eq(system_bh_wq(), queue));
    if kbuild_config::NR_CPUS > 1 {
        let other_cpu = LogicalCpuId::new((cpu_id.as_usize() + 1) % kbuild_config::NR_CPUS);
        let binding = BottomHalfPoolBinding::for_kind_cpu(BottomHalfWorkQueueKind::Default, cpu_id)
            .expect("current CPU BH pool should exist");
        let other_binding =
            BottomHalfPoolBinding::for_kind_cpu(BottomHalfWorkQueueKind::Default, other_cpu)
                .expect("other CPU BH pool should exist");

        assert!(binding.pool().key() != other_binding.pool().key());
    }
}

#[def_test(serial)]
fn test_pool_cpu_intensive_mark_releases_concurrency() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    assert!(pool_state.install_worker(1));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    pool_state.start_running_work(0, 0xA1, WorkInstanceId::for_tests(0xA1), Vec::new());
    assert_eq!(pool_state.nr_running, 1);
    pool_state.runnable_count = 1;

    // The long-running worker already exceeds the zero threshold and leaves
    // nr_running, so the queued work wakes the idle worker.
    let plan = pool_state.select_worker_to_kick();
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);
    assert_eq!(plan.worker_to_wake, Some(WorkerId::new(1)));
    assert!(!plan.should_wake_manager);
    assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);
}

#[def_test(serial)]
fn test_pool_cpu_intensive_tick_releases_concurrency() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    assert!(pool_state.install_worker(1));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    let worker_token =
        pool_state.start_running_work(0, 0xA7, WorkInstanceId::for_tests(0xA7), Vec::new());
    pool_state.runnable_count = 1;

    let plan = pool_state.tick_running_worker(0, worker_token);
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);
    assert_eq!(plan.worker_to_wake, Some(WorkerId::new(1)));
    assert!(!plan.should_wake_manager);
    assert_eq!(pool_state.workers[1].state, WorkerState::Preparing);
}

#[def_test(serial)]
fn test_pool_worker_tick_deadline_tracks_current_execution() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::from_millis(10));

    let stale_token =
        pool_state.start_running_work(0, 0xA8, WorkInstanceId::for_tests(0xA8), Vec::new());
    let stale_deadline = pool_state.worker_tick_deadline(0, stale_token);
    assert!(stale_deadline.is_some());

    let current_token =
        pool_state.start_running_work(0, 0xA9, WorkInstanceId::for_tests(0xA9), Vec::new());
    let current_deadline = pool_state.worker_tick_deadline(0, current_token);
    assert!(current_deadline.is_some());
    assert_eq!(pool_state.worker_tick_deadline(0, stale_token), None);

    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    let _ = pool_state.tick_running_worker(0, current_token);
    assert_eq!(pool_state.worker_tick_deadline(0, current_token), None);
}

#[def_test(serial)]
fn test_worker_execution_token_rejects_stale_finish_and_tick() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    assert!(pool_state.install_worker(1));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);

    let stale_token =
        pool_state.start_running_work(0, 0xA8, WorkInstanceId::for_tests(0xA8), Vec::new());
    let current_token =
        pool_state.start_running_work(0, 0xA9, WorkInstanceId::for_tests(0xA9), Vec::new());
    pool_state.runnable_count = 1;

    assert!(
        pool_state
            .finish_running_work(0, 0xA8, WorkInstanceId::for_tests(0xA8), stale_token)
            .is_empty()
    );
    assert_eq!(pool_state.workers[0].current_work_key, 0xA9);
    assert_eq!(pool_state.nr_running, 1);

    let stale_plan = pool_state.tick_running_worker(0, stale_token);
    assert_eq!(stale_plan, WorkerWakePlan::default());
    assert!(!pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 1);

    let current_plan = pool_state.tick_running_worker(0, current_token);
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);
    assert_eq!(current_plan.worker_to_wake, Some(WorkerId::new(1)));
}

#[def_test(serial)]
fn test_pool_cpu_intensive_mark_is_cleared_per_execution() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    let first_worker_token =
        pool_state.start_running_work(0, 0xA2, WorkInstanceId::for_tests(0xA2), Vec::new());
    pool_state.runnable_count = 1;
    let _ = pool_state.select_worker_to_kick();
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);

    // Finishing a marked work must not decrement nr_running a second time.
    pool_state.runnable_count = 0;
    let _ = pool_state.finish_running_work(
        0,
        0xA2,
        WorkInstanceId::for_tests(0xA2),
        first_worker_token,
    );
    assert_eq!(pool_state.nr_running, 0);
    assert!(!pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.workers[0].state, WorkerState::Idle);

    // The next execution starts with ordinary accounting again.
    let second_worker_token =
        pool_state.start_running_work(0, 0xA3, WorkInstanceId::for_tests(0xA3), Vec::new());
    assert_eq!(pool_state.nr_running, 1);
    assert!(!pool_state.workers[0].cpu_intensive);
    let _ = pool_state.finish_running_work(
        0,
        0xA3,
        WorkInstanceId::for_tests(0xA3),
        second_worker_token,
    );
    assert_eq!(pool_state.nr_running, 0);
}

#[def_test(serial)]
fn test_pool_cpu_intensive_sleep_resume_stays_excluded() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    assert!(pool_state.install_worker(1));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    pool_state.start_running_work(0, 0xA4, WorkInstanceId::for_tests(0xA4), Vec::new());
    pool_state.runnable_count = 1;
    let plan = pool_state.select_worker_to_kick();
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);
    assert_eq!(plan.worker_to_wake, Some(WorkerId::new(1)));

    // Return the kicked worker to idle so the sleep accounting is observable.
    pool_state.runnable_count = 0;
    assert!(!pool_state.prepare_worker_to_wait(1));
    assert_eq!(pool_state.workers[1].state, WorkerState::Idle);

    // Blocking and resuming must not touch nr_running for a marked worker.
    pool_state.runnable_count = 1;
    let transition = pool_state.mark_worker_sleeping(0);
    assert!(transition.did_sleep);
    assert_eq!(pool_state.nr_running, 0);
    assert_eq!(transition.wake_plan.worker_to_wake, Some(WorkerId::new(1)));

    pool_state.mark_worker_running(0);
    assert_eq!(pool_state.workers[0].state, WorkerState::Running);
    assert!(pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 0);
}

#[def_test(serial)]
fn test_pool_cpu_intensive_threshold_not_reached_keeps_count() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    assert!(pool_state.install_worker(1));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::from_secs(60));
    pool_state.start_running_work(0, 0xA5, WorkInstanceId::for_tests(0xA5), Vec::new());
    pool_state.runnable_count = 1;

    // A fresh execution under a large threshold stays in nr_running, so no
    // additional worker is kicked and no marking happens.
    let plan = pool_state.select_worker_to_kick();
    assert!(!pool_state.workers[0].cpu_intensive);
    assert_eq!(pool_state.nr_running, 1);
    assert_eq!(plan.worker_to_wake, None);
    assert!(!plan.should_wake_manager);
}

#[def_test(serial)]
fn test_pool_cpu_intensive_marking_allows_manager_creation() {
    let pool = WorkerPool::new();

    let mut pool_state = pool.state.lock();
    assert!(pool_state.install_worker(0));
    pool_state.set_cpu_intensive_threshold_for_tests(TimeSpan::ZERO);
    pool_state.start_running_work(0, 0xA6, WorkInstanceId::for_tests(0xA6), Vec::new());
    pool_state.runnable_count = 1;

    // The only installed worker is marked, so the pool asks the manager for a
    // new worker instead of waiting behind the CPU-bound callback.
    let plan = pool_state.select_worker_to_kick();
    assert_eq!(plan.worker_to_wake, None);
    assert!(plan.should_wake_manager);
    assert_eq!(pool_state.reserve_worker_creation(), Some(1));
    assert_eq!(pool_state.workers[1].state, WorkerState::Creating);
}

#[def_test(serial)]
fn test_default_and_long_system_queues_share_default_pool() {
    let default_queue = system_wq();
    let long_queue = system_long_wq();
    let bh_queue = system_bh_wq();
    let highpri_bh_queue = system_bh_highpri_wq();
    let default_pwq = WorkQueuePoolBinding::for_static(default_queue)
        .expect("default system queue should resolve");
    let long_pwq =
        WorkQueuePoolBinding::for_static(long_queue).expect("long system queue should resolve");
    let bh_pwq =
        WorkQueuePoolBinding::for_static(bh_queue).expect("BH system queue should resolve");
    let highpri_bh_pwq = WorkQueuePoolBinding::for_static(highpri_bh_queue)
        .expect("high-priority BH queue should resolve");

    assert!(default_queue.key() != long_queue.key());
    assert!(default_queue.key() != bh_queue.key());
    assert!(default_queue.key() != highpri_bh_queue.key());
    assert!(long_queue.key() != bh_queue.key());
    assert!(bh_queue.key() != highpri_bh_queue.key());
    assert_eq!(default_pwq.pool_key(), long_pwq.pool_key());
    assert!(default_pwq.pool_key() != bh_pwq.pool_key());
    assert!(default_pwq.pool_key() != highpri_bh_pwq.pool_key());
    assert!(long_pwq.pool_key() != bh_pwq.pool_key());
    assert!(bh_pwq.pool_key() != highpri_bh_pwq.pool_key());
}

#[def_test(serial)]
fn test_bottom_half_worker_does_not_drain_task_work() {
    let _frozen_pool = FrozenTestPool::freeze_current();
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    let queue = system_wq();
    let work = test_work(count_work);

    WORK_RUNS.store(0, Ordering::Relaxed);
    assert_eq!(queue.queue_work(&work), QueueWorkResult::Queued);

    let binding = BottomHalfPoolBinding::for_kind_cpu(BottomHalfWorkQueueKind::Default, cpu_id)
        .expect("current CPU BH pool should exist");
    assert!(!process_one_bottom_half_pool_work(binding));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);

    assert!(drain_one_test_work(WorkerId::new(0)));
    assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    work.flush()
        .expect("completed task-context work should be flushable");
}
