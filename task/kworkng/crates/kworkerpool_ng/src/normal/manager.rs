// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Normal-service lifecycle, durable management waits and prepared deadline timer.

use alloc::{boxed::Box, sync::Arc, vec};
use core::num::NonZeroUsize;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use ktask::future::PreparedTaskWait;
use ktime_types::{MonotonicInstant, TimeSpan};

use super::{NormalService, PartitionWorkSource, WorkerState};

/// Sole manager-side owner of task references for one prepared normal service.
/// Exclusive access serializes creation/reaping; no asynchronous slot commands
/// escape this owner. The provider runs this owner in a CPU-bound manager task.
pub(crate) struct WorkerManager<S> {
    pub(super) service: Arc<NormalService<S>>,
    tasks: Box<[Option<ktask::KtaskRef>]>,
    idle_for: TimeSpan,
    is_stopping: bool,
}

impl<S: PartitionWorkSource + Send + Sync + 'static> WorkerManager<S> {
    pub(crate) fn new(cpu: LogicalCpuId, limit: NonZeroUsize, idle_for: TimeSpan) -> Self {
        assert!(idle_for > TimeSpan::ZERO, "idle interval must be positive");
        Self {
            service: NormalService::new(cpu, limit),
            tasks: vec![None; limit.get()].into_boxed_slice(),
            idle_for,
            is_stopping: false,
        }
    }

    /// Run in the service CPU's initialized, sleepable manager task. All timer
    /// and task-wait storage is prepared before the manager notification is exposed.
    /// Returns only after a provider stop request and all owned worker exits.
    #[cfg(unittest)]
    pub(crate) fn run(self) {
        self.run_with_startup(None);
    }

    /// Startup notification is owned by this task only until the first worker
    /// has installed its durable wait. No separate persistent ready state exists.
    pub(super) fn run_with_startup(mut self, mut startup: Option<core::task::Waker>) {
        assert_eq!(
            khal::percpu::this_cpu_id(),
            self.service.cpu,
            "wrong manager CPU"
        );
        let mut wait = PreparedTaskWait::prepare().expect("manager needs sleepable task context");
        let wake = wait.waker();
        let timer = ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
            wake.wake_by_ref();
        });
        // Conservative cancellation obligation, not a snapshot of timer pending state.
        // Expiry does not clear it; only this task's synchronous cancellation does.
        let mut timer_needs_cancel = false;
        {
            let mut workers = self.service.workers.lock();
            assert!(workers.manager_waker.is_none(), "manager already installed");
            workers.manager_waker = Some(wait.waker());
        }
        loop {
            let stop_requested = self.service.workers.lock().stop_requested;
            if stop_requested && !self.is_stopping {
                self.stop_if_idle();
            }
            self.run_pass(khal::time::monotonic_time());
            if startup.is_some() && self.service.is_ready() {
                startup.take().unwrap().wake();
            }
            if self.is_stopped() {
                break;
            }
            if let Some(deadline) = self.next_deadline(khal::time::monotonic_time()) {
                // SAFETY: the provider runs this CPU-bound task after scheduler
                // and timer initialization. The owned timer stays alive through
                // cancellation; its handler only invokes a prepared IRQ-safe task
                // waker, never sleeps, enables IRQs or accesses this manager.
                unsafe { timer.start(deadline) }.expect("manager deadline fits timer range");
                timer_needs_cancel = true;
            } else if timer_needs_cancel {
                timer.cancel_sync();
                timer_needs_cancel = false;
            }
            // Source/idle/stop changes after the pass deliver a durable task wake.
            // An earlier wake is consumed here rather than lost before sleeping.
            wait.park();
        }
        // Exclude further timer delivery before withdrawing the task-bound waker.
        drop(timer);
        let wake = self.service.workers.lock().manager_waker.take();
        drop(wake);
    }

    fn next_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant> {
        self.service.dispatch.with_demand(|has_demand| {
            let workers = self.service.workers.lock();
            if workers.pool.has_retiring_workers() {
                // Returning from run_worker precedes published task exit. A
                // positive recheck avoids both missed exit and a manager spin.
                return Some(now + TimeSpan::from_millis(1));
            }
            if has_demand {
                return None;
            }
            workers.pool.next_idle_deadline(self.idle_for)
        })
    }

    /// A task-context pass creates at most one worker and examines at most H task
    /// references. No callback runs here, and runtime operations are unlocked.
    pub(crate) fn run_pass(&mut self, now: MonotonicInstant) {
        assert!(
            ktask::is_sleepable_context(),
            "manager requires task context"
        );
        self.reap_exited();
        if self.is_stopping {
            return;
        }
        self.service.kick();
        let reserved = self.service.dispatch.with_demand(|has_demand| {
            let mut workers = self.service.workers.lock();
            if workers.pool.needs_worker(has_demand) {
                workers.pool.reserve_worker()
            } else {
                None
            }
        });
        if let Some(worker) = reserved {
            let service = self.service.clone();
            let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                move || service.run_worker(worker),
                alloc::format!("kworkng/{}/{}", self.service.cpu.as_usize(), worker),
                kbuild_config::TASK_STACK_SIZE,
            ));
            task.set_cpumask(KCpuMask::one_shot_logical(self.service.cpu));
            assert!(
                self.tasks[worker].is_none(),
                "reserved slot still owns a task"
            );
            self.tasks[worker] = Some(task);
            self.service.workers.lock().pool.worker_prepared(worker);
            // Activation may run the worker immediately. Do not overwrite its
            // phase afterwards, even if the source backlog disappeared meantime.
            ktask::activate_task(self.tasks[worker].as_ref().unwrap());
        }
        for worker in 0..self.tasks.len() {
            let wake = self.service.dispatch.with_demand(|has_demand| {
                if has_demand {
                    return None;
                }
                let mut workers = self.service.workers.lock();
                if workers.pool.request_retirement(worker, now, self.idle_for) {
                    Some(workers.wakers[worker].as_ref().unwrap().clone())
                } else {
                    None
                }
            });
            if let Some(wake) = wake {
                wake.wake();
            }
        }
    }

    /// Reclaim only published exits. Task GC, not this manager, ultimately frees
    /// scheduler-held storage. Dropping our owner precedes capacity reuse.
    fn reap_exited(&mut self) {
        for (worker, task) in self.tasks.iter_mut().enumerate() {
            if task
                .as_ref()
                .is_some_and(|task| task.state() == ktask::TaskState::Exited)
            {
                drop(task.take());
                self.service.worker_exited(worker);
            }
        }
    }

    /// Whole-service teardown only, never Queue shutdown. The provider must have
    /// stopped producers and drained/detached sources. Refuses without effects
    /// while startup, selection or execution remains. After success, run passes
    /// until is_stopped; no live runtime resource is destroyed under protection.
    pub(crate) fn stop_if_idle(&mut self) -> bool {
        assert!(
            ktask::is_sleepable_context(),
            "manager requires task context"
        );
        let can_stop = self.service.dispatch.with_demand(|has_demand| {
            if has_demand {
                return false;
            }
            let mut workers = self.service.workers.lock();
            if (0..self.tasks.len()).any(|id| {
                !matches!(
                    workers.pool.state(id),
                    WorkerState::Empty | WorkerState::Parked { .. } | WorkerState::Retiring
                )
            }) {
                return false;
            }
            for worker in 0..self.tasks.len() {
                if matches!(workers.pool.state(worker), WorkerState::Parked { .. }) {
                    workers.pool.retire_parked_worker(worker);
                }
            }
            true
        });
        if !can_stop {
            return false;
        }
        self.is_stopping = true;
        for worker in 0..self.tasks.len() {
            let wake = self.service.workers.lock().wakers[worker].clone();
            if let Some(wake) = wake {
                wake.wake();
            }
        }
        true
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.is_stopping && self.tasks.iter().all(Option::is_none)
    }
}

impl<S> Drop for WorkerManager<S> {
    fn drop(&mut self) {
        assert!(
            self.tasks.iter().all(Option::is_none),
            "manager must retire and reap its workers"
        );
    }
}

unittest::path_mod_test!(
    manager_unittest,
    "../../ut-tests/normal_manager_unittest.rs"
);
