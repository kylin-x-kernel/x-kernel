// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ktask` provider for the KIRQ system workerqueue.

use alloc::string::String;
use core::{future::poll_fn, task::Poll};

use kcpu_id_map::KCpuMaskExt;
use kpoll::{PollRegistrations, PollSet};
use kspin::{NoPreempt, SpinNoIrq};

use crate::{KCpuMask, TaskInner, activate_task, future::block_on, prepare_task, yield_now};

static SYSTEM_WORKER: SpinNoIrq<Option<SystemWorker>> = SpinNoIrq::new(None);

struct SystemWorker {
    wake_source: PollSet,
}

/// Starts the system workerqueue worker if it has not been started yet.
///
/// M4 uses one `kworker/system_wq` task to preserve the current KIRQ
/// single-consumer queue invariant. Later workerqueue milestones can replace
/// this with per-CPU or pooled workers after the queue state model is widened.
pub fn init_system_workqueue_worker() {
    let wake_source = PollSet::new();
    if !install_system_worker(wake_source.clone()) {
        return;
    }

    let cpu_id = khal::percpu::this_cpu_id();
    let task = TaskInner::new_pidless_kthread(
        move || system_worker_main(wake_source),
        String::from("kworker/system_wq"),
        kbuild_config::TASK_STACK_SIZE,
    );
    let task = prepare_task(task);
    task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    activate_task(&task);
}

#[kiface::provide]
impl kirq::workerqueue::WorkerqueueHostIf {
    fn wake_system_worker() {
        let _guard = NoPreempt::new();
        if let Some(source) = system_worker_wake_source() {
            let _ = source.wake();
        }
    }
}

#[kiface::provide]
impl kirq::workerqueue::WorkerqueueTaskContextIf {
    fn set_current_work_context(
        context: kirq::workerqueue::WorkerqueueTaskContext,
    ) -> Option<kirq::workerqueue::WorkerqueueTaskContext> {
        crate::current_may_uninit().as_ref().and_then(|current| {
            current
                .set_workerqueue_current_work_context(context.work_key(), context.queue_key())
                .map(|(work_key, queue_key)| {
                    kirq::workerqueue::WorkerqueueTaskContext::new(work_key, queue_key)
                })
        })
    }

    fn clear_current_work_context(context: kirq::workerqueue::WorkerqueueTaskContext) -> bool {
        crate::current_may_uninit().as_ref().is_some_and(|current| {
            current.clear_workerqueue_current_work_context(context.work_key(), context.queue_key())
        })
    }

    fn current_work_context() -> Option<kirq::workerqueue::WorkerqueueTaskContext> {
        crate::current_may_uninit().as_ref().and_then(|current| {
            current
                .workerqueue_current_work_context()
                .map(|(work_key, queue_key)| {
                    kirq::workerqueue::WorkerqueueTaskContext::new(work_key, queue_key)
                })
        })
    }
}

#[kiface::provide]
impl kirq::workerqueue::WorkqueueSyncWaitIf {
    fn wait_for_completion(completion: &kpoll::Completion) -> Result<(), kpoll::PollRegisterError> {
        crate::irq_wait::wait_for_completion(completion)
    }
}

fn install_system_worker(wake_source: PollSet) -> bool {
    let mut slot = SYSTEM_WORKER.lock();
    if slot.is_some() {
        return false;
    }
    *slot = Some(SystemWorker { wake_source });
    true
}

fn system_worker_wake_source() -> Option<PollSet> {
    SYSTEM_WORKER
        .lock()
        .as_ref()
        .map(|worker| worker.wake_source.clone())
}

fn system_worker_main(wake_source: PollSet) {
    debug!("started kworker/system_wq");
    loop {
        wait_for_system_work(&wake_source);
        drain_system_workqueue();
    }
}

fn drain_system_workqueue() {
    while kirq::workerqueue::run_one_work(kirq::workerqueue::system_wq()) {
        yield_now();
    }
}

fn wait_for_system_work(wake_source: &PollSet) {
    enum WaitResult {
        PendingReady,
        RetryAfterYield,
    }

    let mut registrations = PollRegistrations::new();
    loop {
        match block_on(poll_fn(|cx| {
            if kirq::workerqueue::system_wq_has_runnable_work() {
                return Poll::Ready(WaitResult::PendingReady);
            }

            let mut context = registrations.context(cx);
            if let Err(error) = context.register(wake_source) {
                warn!("failed to register system workerqueue waiter: {error:?}");
                drop(context);
                return Poll::Ready(WaitResult::RetryAfterYield);
            }
            drop(context);

            if kirq::workerqueue::system_wq_has_runnable_work() {
                return Poll::Ready(WaitResult::PendingReady);
            }
            Poll::Pending
        })) {
            WaitResult::PendingReady => break,
            WaitResult::RetryAfterYield => yield_now(),
        }
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;

    static SYSTEM_WORKER_RUNS: AtomicUsize = AtomicUsize::new(0);
    static LONG_WORK_STARTED: AtomicUsize = AtomicUsize::new(0);
    static LONG_WORK_CAN_FINISH: AtomicUsize = AtomicUsize::new(0);
    static FLUSH_FINISHED: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_FINISHED: AtomicUsize = AtomicUsize::new(0);
    static REQUEUE_RESULT: AtomicUsize = AtomicUsize::new(0);
    static SELF_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);

    fn count_system_work(_work: &kirq::workerqueue::WorkItem) {
        SYSTEM_WORKER_RUNS.fetch_add(1, Ordering::Release);
    }

    fn long_system_work(_work: &kirq::workerqueue::WorkItem) {
        LONG_WORK_STARTED.store(1, Ordering::Release);
        while LONG_WORK_CAN_FINISH.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    fn requeue_then_wait_system_work(work: &kirq::workerqueue::WorkItem) {
        LONG_WORK_STARTED.store(1, Ordering::Release);
        let code = match kirq::workerqueue::schedule_work(work) {
            kirq::workerqueue::QueueWorkResult::Queued => 1,
            kirq::workerqueue::QueueWorkResult::AlreadyQueued => 2,
            kirq::workerqueue::QueueWorkResult::Disabled => 3,
            kirq::workerqueue::QueueWorkResult::QueueFull => 4,
        };
        REQUEUE_RESULT.store(code, Ordering::Release);
        while LONG_WORK_CAN_FINISH.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    fn self_flush_work(work: &kirq::workerqueue::WorkItem) {
        let result = kirq::workerqueue::flush_work(work);
        let code = match result {
            Err(kirq::workerqueue::WorkqueueError::SelfWait) => 2,
            Ok(_) => 1,
            Err(_) => 3,
        };
        SELF_WAIT_RESULT.store(code, Ordering::Release);
    }

    #[def_test(serial)]
    fn test_system_worker_drains_scheduled_work() {
        init_system_workqueue_worker();

        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let work = kirq::workerqueue::WorkItem::new(count_system_work);

        assert_eq!(
            kirq::workerqueue::schedule_work(&work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        for _ in 0..128 {
            if SYSTEM_WORKER_RUNS.load(Ordering::Acquire) == before + 1 {
                break;
            }
            yield_now();
        }

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
    }

    #[def_test(serial)]
    fn test_flush_work_waits_for_system_worker_callback() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        FLUSH_FINISHED.store(0, Ordering::Release);

        let work = kirq::workerqueue::WorkItem::new(long_system_work);
        assert_eq!(
            kirq::workerqueue::schedule_work(&work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        crate::spawn(move || {
            kirq::workerqueue::flush_work(&work).expect("flush_work should wait in task context");
            FLUSH_FINISHED.store(1, Ordering::Release);
        });

        for _ in 0..16 {
            yield_now();
        }
        assert_eq!(FLUSH_FINISHED.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while FLUSH_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    #[def_test(serial)]
    fn test_flush_work_waits_for_pending_private_work() {
        let queue = Box::leak(Box::new(kirq::workerqueue::WorkQueue::new("test")));

        SYSTEM_WORKER_RUNS.store(0, Ordering::Release);
        FLUSH_FINISHED.store(0, Ordering::Release);

        let work = kirq::workerqueue::WorkItem::new(count_system_work);
        assert_eq!(
            kirq::workerqueue::queue_work(queue, &work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        let flush_work = work.clone();
        crate::spawn(move || {
            if kirq::workerqueue::flush_work(&flush_work) != Ok(true) {
                panic!("flush_work should wait for pending private work");
            }
            FLUSH_FINISHED.store(1, Ordering::Release);
        });

        for _ in 0..16 {
            yield_now();
        }
        assert_eq!(FLUSH_FINISHED.load(Ordering::Acquire), 0);

        assert_eq!(usize::from(kirq::workerqueue::run_one_work(queue)), 1);
        while FLUSH_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), 1);
    }

    #[def_test(serial)]
    fn test_cancel_work_sync_waits_for_running_system_work() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        CANCEL_FINISHED.store(0, Ordering::Release);

        let work = kirq::workerqueue::WorkItem::new(long_system_work);
        assert_eq!(
            kirq::workerqueue::schedule_work(&work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        let cancel_work = work.clone();
        crate::spawn(move || {
            kirq::workerqueue::cancel_work_sync(&cancel_work)
                .expect("cancel_work_sync should wait in task context");
            CANCEL_FINISHED.store(1, Ordering::Release);
        });

        for _ in 0..16 {
            yield_now();
        }
        assert_eq!(CANCEL_FINISHED.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while CANCEL_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    #[def_test(serial)]
    fn test_cancel_work_sync_removes_running_followup_and_disables_queueing() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        CANCEL_FINISHED.store(0, Ordering::Release);
        REQUEUE_RESULT.store(0, Ordering::Release);

        let work = kirq::workerqueue::WorkItem::new(requeue_then_wait_system_work);
        assert_eq!(
            kirq::workerqueue::schedule_work(&work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        while REQUEUE_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(REQUEUE_RESULT.load(Ordering::Acquire), 1);

        let cancel_work = work.clone();
        crate::spawn(move || {
            kirq::workerqueue::cancel_work_sync(&cancel_work)
                .expect("cancel_work_sync should wait in task context");
            CANCEL_FINISHED.store(1, Ordering::Release);
        });

        let mut saw_disabled = false;
        for _ in 0..128 {
            if kirq::workerqueue::schedule_work(&work)
                == kirq::workerqueue::QueueWorkResult::Disabled
            {
                saw_disabled = true;
                break;
            }
            yield_now();
        }
        assert_eq!(usize::from(saw_disabled), 1);
        assert_eq!(CANCEL_FINISHED.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while CANCEL_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    #[def_test(serial)]
    fn test_flush_work_rejects_self_wait_from_worker_callback() {
        init_system_workqueue_worker();

        SELF_WAIT_RESULT.store(0, Ordering::Release);
        let work = kirq::workerqueue::WorkItem::new(self_flush_work);
        assert_eq!(
            kirq::workerqueue::schedule_work(&work),
            kirq::workerqueue::QueueWorkResult::Queued
        );

        while SELF_WAIT_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(SELF_WAIT_RESULT.load(Ordering::Acquire), 2);
    }
}
