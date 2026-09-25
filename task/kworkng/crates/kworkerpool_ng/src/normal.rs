// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private normal workers and their shared manager notification state.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{num::NonZeroUsize, task::Waker};

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktask::future::PreparedTaskWait;
pub use runtime::RuntimeError as NormalRuntimeError;
pub use services::NormalServices;

use crate::{
    AcquiredExecution, PartitionDispatch, PartitionWorkSource, pool::WorkerPool,
    worker::WorkerState,
};

mod manager;
mod runtime;
mod services;
mod wake;

/// Source demand and worker parking use dispatch -> workers lock order.
/// Runtime references remain owned by the preparing manager, not by this service.
pub(crate) struct NormalService<S> {
    cpu: LogicalCpuId,
    dispatch: Arc<PartitionDispatch<S>>,
    workers: SpinNoIrq<NormalWorkers>,
    notification: Arc<Waker>,
}

struct NormalWorkers {
    pool: WorkerPool,
    // Task-bound waits cannot be prepared before the corresponding task starts.
    // These weak-task wakers are installed before that slot becomes Available.
    wakers: Box<[Option<Waker>]>,
    manager_waker: Option<Waker>,
    stop_requested: bool,
}

impl<S: Send + Sync + 'static> NormalService<S> {
    pub(crate) fn new(cpu: LogicalCpuId, limit: NonZeroUsize) -> Arc<Self> {
        assert!(kcpu_id_map::raw_cpu_id(cpu).is_some(), "invalid worker CPU");
        Arc::new_cyclic(|owner| Self {
            cpu,
            dispatch: PartitionDispatch::new(),
            workers: SpinNoIrq::new(NormalWorkers {
                pool: WorkerPool::new(limit),
                wakers: vec![None; limit.get()].into_boxed_slice(),
                manager_waker: None,
                stop_requested: false,
            }),
            notification: wake::prepare(owner.clone()),
        })
    }
}

impl<S> NormalService<S> {
    /// Independent notification capability; producers publish source demand first.
    /// The runtime owner must retain this service while accepted demand exists.
    pub(crate) fn notification_waker(&self) -> Arc<Waker> {
        self.notification.clone()
    }

    /// Select a parked worker while demand publication is excluded. Cloning the
    /// prepared task waker does not allocate; scheduler entry is after unlocking.
    pub(crate) fn kick(&self) {
        let wake = self.dispatch.with_demand(|has_demand| {
            if !has_demand {
                return None;
            }
            let mut workers = self.workers.lock();
            for worker in 0..workers.wakers.len() {
                if workers.pool.wake_worker(worker) {
                    return Some(workers.wakers[worker].as_ref().unwrap().clone());
                }
            }
            if workers.pool.needs_worker(has_demand) {
                workers.manager_waker.clone()
            } else {
                None
            }
        });
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    fn wake_manager(&self) {
        let wake = self.workers.lock().manager_waker.clone();
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    /// Whole-provider teardown request after producers stop and sources drain.
    /// The request survives manager startup; it does not stop a single Queue.
    pub(crate) fn request_stop(&self) {
        let wake = {
            let mut workers = self.workers.lock();
            workers.stop_requested = true;
            workers.manager_waker.clone()
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }

    /// Runtime must join the task and drop its owning reference before this call.
    /// No old manager command may retain permission to mutate the reused slot.
    pub(crate) fn worker_exited(&self, worker: usize) {
        let waker = {
            let mut workers = self.workers.lock();
            workers.pool.worker_exited(worker);
            workers.wakers[worker].take()
        };
        drop(waker);
    }
}

impl<S: PartitionWorkSource> NormalService<S> {
    /// Enter only from the prepared CPU-bound worker task for this reserved slot.
    /// Preparation allocates once; source acquisition, execution and parking do not
    /// allocate here. The task returns only after a manager requests retirement.
    pub(crate) fn run_worker(&self, worker: usize) {
        self.run_worker_before_park(worker, || {});
    }

    fn run_worker_before_park(&self, worker: usize, mut before_park: impl FnMut()) {
        assert_eq!(khal::percpu::this_cpu_id(), self.cpu, "wrong worker CPU");
        let mut wait =
            PreparedTaskWait::prepare().expect("worker starts in sleepable task context");
        {
            let mut workers = self.workers.lock();
            assert!(
                workers.wakers[worker].is_none(),
                "worker wait already installed"
            );
            workers.wakers[worker] = Some(wait.waker());
            workers.pool.worker_started(worker);
        }
        loop {
            // A spurious task wake is not service availability. Recheck the
            // protected phase; only demand or retirement may end logical parking.
            let mut became_parked = false;
            let phase = self.dispatch.with_demand(|has_demand| {
                let mut workers = self.workers.lock();
                if workers.pool.state(worker) == WorkerState::Retiring {
                    return WorkerState::Retiring;
                }
                if has_demand {
                    workers.pool.wake_worker(worker);
                    assert!(workers.pool.begin_selection(worker));
                    WorkerState::Selecting
                } else {
                    if workers.pool.state(worker) == WorkerState::Available {
                        workers
                            .pool
                            .park_worker(worker, khal::time::monotonic_time());
                        became_parked = true;
                    }
                    workers.pool.state(worker)
                }
            });
            if became_parked {
                self.wake_manager();
            }
            if phase == WorkerState::Retiring {
                return;
            }
            if matches!(phase, WorkerState::Parked { .. }) {
                before_park();
                wait.park();
                continue;
            }
            let Some(inspection) = self.dispatch.begin_inspection() else {
                self.workers.lock().pool.finish_selection(worker, false);
                continue;
            };
            let (execution, remainder) = inspection.source().acquire();
            inspection.finish(remainder);
            self.workers
                .lock()
                .pool
                .finish_selection(worker, execution.is_some());
            // Requeued demand can wake another worker before this callback sleeps.
            self.kick();
            if let Some(execution) = execution {
                execution.run();
                self.workers.lock().pool.finish_execution(worker);
            }
        }
    }
}

#[cfg(unittest)]
mod tests;
