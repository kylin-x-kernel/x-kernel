// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ/BH context provider for the generic `kwork` workerqueue core.
//!
//! Workerqueue state and lifecycle rules are owned by `kwork`. `kirq` only
//! provides the interrupt-like context predicate required by sleepable
//! workerqueue synchronization APIs.

use ktime_types::{MonotonicInstant, TimeSpan};
use kwork::raw::{
    BottomHalfPoolBinding, BottomHalfWorkQueueKind, WorkqueueBottomHalfIf, WorkqueueHostIf,
    WorkqueueTimerIf,
};

use super::softirq::{SoftirqVec, open_softirq, raise_softirq};

const BH_WORKER_TIMESLICE: TimeSpan = TimeSpan::from_millis(2);
const BH_WORKER_RESTARTS: usize = 10;

/// Installs bottom-half workerqueue softirq actions.
///
/// Linux routes `system_bh_wq` and `system_bh_highpri_wq` through softirq
/// execution. X-Kernel keeps the same boundary: `kwork` owns queue/pool
/// accounting and `kirq` owns the non-sleepable softirq drain context.
pub fn init() -> bool {
    let default_installed = open_softirq(SoftirqVec::Tasklet, drain_default_bh_workqueue);
    let highpri_installed = open_softirq(SoftirqVec::High, drain_highpri_bh_workqueue);
    default_installed && highpri_installed
}

#[kiface::provide]
impl kwork::raw::WorkqueueContextIf {
    fn is_invalid_wait_context() -> bool {
        crate::context::is_in_interrupt_context()
    }
}

#[kiface::provide]
impl WorkqueueBottomHalfIf {
    fn raise_bottom_half(kind: BottomHalfWorkQueueKind) {
        raise_softirq(softirq_vec_for_kind(kind));
    }
}

fn drain_default_bh_workqueue() {
    drain_bh_workqueue(BottomHalfWorkQueueKind::Default);
}

fn drain_highpri_bh_workqueue() {
    drain_bh_workqueue(BottomHalfWorkQueueKind::HighPri);
}

fn drain_bh_workqueue(kind: BottomHalfWorkQueueKind) {
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    let Some(binding) = BottomHalfPoolBinding::for_kind_cpu(kind, cpu_id) else {
        return;
    };
    let mut budget = BottomHalfDrainBudget::new();

    loop {
        if !kwork::raw::process_one_bottom_half_pool_work(binding) {
            return;
        }
        budget.record_completed_work();
        if !budget.can_continue() {
            break;
        }
    }

    if binding.has_runnable_work() {
        raise_softirq(softirq_vec_for_kind(kind));
    }
}

struct BottomHalfDrainBudget {
    restarts_left: usize,
    deadline: MonotonicInstant,
}

impl BottomHalfDrainBudget {
    fn new() -> Self {
        let now = WorkqueueTimerIf::monotonic_time();
        Self {
            restarts_left: BH_WORKER_RESTARTS,
            deadline: now
                .checked_add(BH_WORKER_TIMESLICE)
                .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX)),
        }
    }

    fn record_completed_work(&mut self) {
        self.restarts_left = self.restarts_left.saturating_sub(1);
    }

    fn can_continue(&self) -> bool {
        self.restarts_left > 0 && WorkqueueTimerIf::monotonic_time() < self.deadline
    }
}

const fn softirq_vec_for_kind(kind: BottomHalfWorkQueueKind) -> SoftirqVec {
    match kind {
        BottomHalfWorkQueueKind::Default => SoftirqVec::Tasklet,
        BottomHalfWorkQueueKind::HighPri => SoftirqVec::High,
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::{boxed::Box, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kspin::SpinNoIrq;
    use ktime_types::TimeSpan;
    use kwork::raw::{
        DelayedScheduledWork, QueueDelayedWorkResult, QueueWorkResult, ScheduleAttrs,
        ScheduledWork, SystemPoolBinding, SystemWorkQueueKind, WorkQueue, WorkQueueAttrs,
        WorkQueueStartError, WorkqueueError, WorkqueueHostIf, system_bh_highpri_wq, system_bh_wq,
    };
    use unittest::{assert_eq, def_test};

    use crate::{
        context::{HardIrqContextGuard, local_bh_disable},
        softirq::{
            SoftirqRunResult, SoftirqVec, raise_softirq, softirq_diagnostics,
            test_support::ScopedSoftirqAction,
        },
    };

    static WORK_RUNS: AtomicUsize = AtomicUsize::new(0);
    static BH_FLUSH_RESULT: AtomicUsize = AtomicUsize::new(0);
    static BH_CANCEL_SYNC_RESULT: AtomicUsize = AtomicUsize::new(0);
    static SOFTIRQ_QUEUE_RESULT: AtomicUsize = AtomicUsize::new(0);
    static SOFTIRQ_TEST_QUEUE: SpinNoIrq<Option<&'static WorkQueue>> = SpinNoIrq::new(None);
    static BH_WAIT_TARGET: SpinNoIrq<Option<ScheduledWork>> = SpinNoIrq::new(None);

    struct ScopedTestQueue {
        slot: &'static SpinNoIrq<Option<&'static WorkQueue>>,
        previous: Option<&'static WorkQueue>,
    }

    impl ScopedTestQueue {
        fn install(
            slot: &'static SpinNoIrq<Option<&'static WorkQueue>>,
            queue: &'static WorkQueue,
        ) -> Self {
            let previous = slot.lock().replace(queue);
            Self { slot, previous }
        }
    }

    impl Drop for ScopedTestQueue {
        fn drop(&mut self) {
            *self.slot.lock() = self.previous;
        }
    }

    struct ScopedTestWork {
        previous: Option<ScheduledWork>,
    }

    impl ScopedTestWork {
        fn install(work: ScheduledWork) -> Self {
            let previous = BH_WAIT_TARGET.lock().replace(work);
            Self { previous }
        }
    }

    impl Drop for ScopedTestWork {
        fn drop(&mut self) {
            *BH_WAIT_TARGET.lock() = self.previous.take();
        }
    }

    fn count_work(_work: &ScheduledWork) {
        WORK_RUNS.fetch_add(1, Ordering::Relaxed);
    }

    fn count_softirq_work(_work: &ScheduledWork) {
        if !crate::context::is_serving_softirq() {
            panic!("BH workerqueue callback should run in softirq context");
        }
        WORK_RUNS.fetch_add(1, Ordering::Relaxed);
    }

    fn record_bh_wait_api_results(_work: &ScheduledWork) {
        let target = BH_WAIT_TARGET
            .lock()
            .as_ref()
            .expect("BH wait target should be installed")
            .clone();

        BH_FLUSH_RESULT.store(
            workqueue_wait_result_code(target.flush()),
            Ordering::Release,
        );
        BH_CANCEL_SYNC_RESULT.store(
            workqueue_wait_result_code(target.cancel_sync()),
            Ordering::Release,
        );
    }

    fn workqueue_wait_result_code(result: Result<bool, WorkqueueError>) -> usize {
        match result {
            Err(WorkqueueError::InvalidContext) => 1,
            Ok(_) => 2,
            Err(_) => 3,
        }
    }

    fn queue_result_code(result: QueueWorkResult) -> usize {
        match result {
            QueueWorkResult::Queued => 1,
            QueueWorkResult::WorkerUnavailable => 2,
            QueueWorkResult::AlreadyQueued => 3,
            QueueWorkResult::QueueFull => 4,
            QueueWorkResult::Disabled => 5,
            QueueWorkResult::InvalidCpu => 6,
        }
    }

    fn assert_irq_safe_queue_result(result: QueueWorkResult) {
        match result {
            QueueWorkResult::Queued | QueueWorkResult::WorkerUnavailable => {}
            result => panic!(
                "IRQ-safe enqueue should only depend on worker-pool readiness, got {result:?}"
            ),
        }
    }

    fn queue_from_softirq_action() {
        let queue = SOFTIRQ_TEST_QUEUE
            .lock()
            .expect("softirq test queue should be installed");
        let work = ScheduledWork::new(count_work);
        SOFTIRQ_QUEUE_RESULT.store(
            queue_result_code(queue.queue_work(&work)),
            Ordering::Release,
        );
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_in_hardirq_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = ScheduledWork::new(count_work);
        // Freeze the current CPU's system pool so a live worker cannot drain
        // the work before the hardirq window has been observed; the queued
        // kick is buffered and executed at unfreeze.
        let pool = SystemPoolBinding::for_kind_cpu(
            SystemWorkQueueKind::Default,
            WorkqueueHostIf::current_cpu_id(),
        )
        .expect("current CPU should have a default worker pool");
        pool.freeze_wakes_for_tests();
        WORK_RUNS.store(0, Ordering::Relaxed);

        let result = {
            let _hardirq = HardIrqContextGuard::enter();
            let result = queue.queue_work(&work);
            assert_irq_safe_queue_result(result);
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
            result
        };
        if result == QueueWorkResult::WorkerUnavailable {
            pool.unfreeze_wakes_for_tests(true);
        } else {
            pool.unfreeze_wakes_for_tests(false);
            work.flush().expect("hardirq-queued work should finish");
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        }
    }

    #[def_test(serial)]
    fn test_waiting_work_apis_reject_hardirq_context() {
        let work = ScheduledWork::new(count_work);

        let _hardirq = HardIrqContextGuard::enter();
        assert_eq!(work.flush(), Err(WorkqueueError::InvalidContext));
        assert_eq!(work.cancel_sync(), Err(WorkqueueError::InvalidContext));
    }

    #[def_test(serial)]
    fn test_nonzero_delayed_work_rejects_hardirq_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let delayed = DelayedScheduledWork::new(count_work);

        let _hardirq = HardIrqContextGuard::enter();
        assert_eq!(
            queue.queue_delayed_work(&delayed, TimeSpan::from_secs(1)),
            QueueDelayedWorkResult::InvalidContext
        );
        assert_eq!(
            queue.mod_delayed_work(&delayed, TimeSpan::from_secs(1)),
            QueueDelayedWorkResult::InvalidContext
        );
    }

    #[def_test(serial)]
    fn test_zero_delay_delayed_work_is_allowed_in_hardirq_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let delayed = DelayedScheduledWork::new(count_work);
        let pool = SystemPoolBinding::for_kind_cpu(
            SystemWorkQueueKind::Default,
            WorkqueueHostIf::current_cpu_id(),
        )
        .expect("current CPU should have a default worker pool");
        pool.freeze_wakes_for_tests();
        WORK_RUNS.store(0, Ordering::Relaxed);

        let result = {
            let _hardirq = HardIrqContextGuard::enter();
            let result = queue.queue_delayed_work(&delayed, TimeSpan::ZERO);
            match result {
                QueueDelayedWorkResult::Queued | QueueDelayedWorkResult::WorkerUnavailable => {}
                result => panic!(
                    "IRQ-safe zero-delay enqueue should only depend on worker-pool readiness, got \
                     {result:?}"
                ),
            }
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
            result
        };
        if result == QueueDelayedWorkResult::WorkerUnavailable {
            pool.unfreeze_wakes_for_tests(true);
        } else {
            pool.unfreeze_wakes_for_tests(false);
            delayed
                .flush()
                .expect("hardirq zero-delay delayed work should finish");
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        }
    }

    #[def_test(serial)]
    fn test_start_workqueue_rejects_hardirq_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));

        let _hardirq = HardIrqContextGuard::enter();
        assert_eq!(
            queue.start(WorkQueueAttrs::new()),
            Err(WorkQueueStartError::InvalidContext)
        );
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_with_bh_disabled() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = ScheduledWork::new(count_work);
        WORK_RUNS.store(0, Ordering::Relaxed);

        let result = {
            let _bh = local_bh_disable();
            let result = queue.queue_work(&work);
            assert_irq_safe_queue_result(result);
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
            result
        };
        if result == QueueWorkResult::Queued {
            work.flush().expect("BH-disabled queued work should finish");
            assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        }
    }

    #[def_test(serial)]
    fn test_nonzero_delayed_work_rejects_bh_disabled_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let delayed = DelayedScheduledWork::new(count_work);

        let _bh = local_bh_disable();
        assert_eq!(
            queue.queue_delayed_work(&delayed, TimeSpan::from_secs(1)),
            QueueDelayedWorkResult::InvalidContext
        );
        assert_eq!(
            queue.mod_delayed_work(&delayed, TimeSpan::from_secs(1)),
            QueueDelayedWorkResult::InvalidContext
        );
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_from_softirq_action() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let _queue = ScopedTestQueue::install(&SOFTIRQ_TEST_QUEUE, queue);
        let _action = ScopedSoftirqAction::install(SoftirqVec::Block, queue_from_softirq_action);
        SOFTIRQ_QUEUE_RESULT.store(0, Ordering::Release);

        raise_softirq(SoftirqVec::Block);
        let _ = crate::softirq::run_pending_softirqs();

        let result = match SOFTIRQ_QUEUE_RESULT.load(Ordering::Acquire) {
            1 => QueueWorkResult::Queued,
            2 => QueueWorkResult::WorkerUnavailable,
            3 => QueueWorkResult::AlreadyQueued,
            4 => QueueWorkResult::QueueFull,
            5 => QueueWorkResult::Disabled,
            6 => QueueWorkResult::InvalidCpu,
            _ => panic!("softirq action did not record a queue result"),
        };
        assert_irq_safe_queue_result(result);
        if result == QueueWorkResult::Queued {
            assert_eq!(queue.flush(), Ok(()));
        }
    }

    #[def_test(serial)]
    fn test_system_bh_wq_drains_in_softirq_context() {
        let _softirq = crate::softirq::test_support::begin_softirq_test();
        let work = ScheduledWork::new(count_softirq_work);
        WORK_RUNS.store(0, Ordering::Relaxed);

        assert_eq!(system_bh_wq().queue_work(&work), QueueWorkResult::Queued);
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);

        let _ = crate::softirq::run_pending_softirqs();
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        work.flush()
            .expect("completed BH work should be flushable from task context");
    }

    #[def_test(serial)]
    fn test_scheduled_work_schedules_bottom_half_instance() {
        let _softirq = crate::softirq::test_support::begin_softirq_test();
        let scheduled = ScheduledWork::new(count_softirq_work);
        WORK_RUNS.store(0, Ordering::Relaxed);

        assert_eq!(
            scheduled.schedule_with(ScheduleAttrs::bottom_half()),
            QueueWorkResult::Queued
        );
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);

        let _ = crate::softirq::run_pending_softirqs();
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        scheduled
            .flush()
            .expect("completed scheduled BH work should be flushable from task context");
    }

    #[def_test(serial)]
    fn test_system_bh_highpri_wq_drains_in_softirq_context() {
        let _softirq = crate::softirq::test_support::begin_softirq_test();
        let work = ScheduledWork::new(count_softirq_work);
        WORK_RUNS.store(0, Ordering::Relaxed);

        assert_eq!(
            system_bh_highpri_wq().queue_work(&work),
            QueueWorkResult::Queued
        );
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);

        let _ = crate::softirq::run_pending_softirqs();
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        work.flush()
            .expect("completed high-priority BH work should be flushable from task context");
    }

    #[def_test(serial)]
    fn test_system_bh_wq_callback_rejects_waiting_work_apis() {
        let _softirq = crate::softirq::test_support::begin_softirq_test();
        let target = ScheduledWork::new(count_work);
        let _target = ScopedTestWork::install(target);
        let work = ScheduledWork::new(record_bh_wait_api_results);
        BH_FLUSH_RESULT.store(0, Ordering::Relaxed);
        BH_CANCEL_SYNC_RESULT.store(0, Ordering::Relaxed);

        assert_eq!(system_bh_wq().queue_work(&work), QueueWorkResult::Queued);
        let _ = crate::softirq::run_pending_softirqs();

        assert_eq!(BH_FLUSH_RESULT.load(Ordering::Acquire), 1);
        assert_eq!(BH_CANCEL_SYNC_RESULT.load(Ordering::Acquire), 1);
        work.flush()
            .expect("completed BH wait-gate test work should be flushable from task context");
    }

    #[def_test(serial)]
    fn test_system_bh_wq_restarts_after_worker_budget() {
        let _softirq = crate::softirq::test_support::begin_softirq_test();
        let mut works = Vec::new();
        WORK_RUNS.store(0, Ordering::Relaxed);

        for _ in 0..=super::BH_WORKER_RESTARTS {
            let work = ScheduledWork::new(count_softirq_work);
            assert_eq!(system_bh_wq().queue_work(&work), QueueWorkResult::Queued);
            works.push(work);
        }

        assert_eq!(
            crate::softirq::run_pending_softirqs(),
            SoftirqRunResult::Ran
        );
        assert_eq!(
            WORK_RUNS.load(Ordering::Relaxed),
            super::BH_WORKER_RESTARTS + 1
        );
        assert!(
            softirq_diagnostics().runs >= 2,
            "BH workerqueue should re-raise softirq after exhausting its per-action budget"
        );

        for work in works {
            work.flush()
                .expect("completed BH budget test work should be flushable");
        }
    }
}
