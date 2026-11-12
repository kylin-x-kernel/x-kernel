// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private normal workers and their shared manager notification state.

use alloc::sync::Arc;
use core::{num::NonZeroUsize, task::Waker};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
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

/// Cooperative PM participation for one complete normal-worker service set.
/// This policy is fixed before task publication, not changed by queue aliases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NormalFreezePolicy {
    /// Existing behavior: no loop checkpoints; PM must exempt and separately
    /// quiesce these tasks, or freezing them will time out.
    KeepRunning,
    /// Managers and workers confirm freezes at unlocked loop/idle boundaries.
    /// Use only for dedicated sources whose callbacks and cleanup finish without
    /// other frozen tasks. PM/RTC/recovery work must use separate services.
    Cooperative,
}

impl NormalFreezePolicy {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: provider loop runs in its initialized \
                  sleepable kernel task, outside dispatch/worker locks and callback ownership"
    )]
    fn checkpoint(self) {
        if self == Self::Cooperative {
            ktask::freezer::checkpoint().expect("worker safe point remains sleepable");
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: prepared wait belongs to this initialized \
                  kernel task and provider releases all selection/worker locks before parking"
    )]
    fn park(self, wait: &mut PreparedTaskWait) {
        if self == Self::Cooperative {
            wait.park_freezable()
                .expect("worker wait remains sleepable");
        } else {
            wait.park();
        }
    }
}

/// Source demand and worker parking use dispatch -> workers lock order.
/// Runtime references remain owned by the preparing manager, not by this service.
pub(crate) struct NormalService<S> {
    cpu: LogicalCpuId,
    dispatch: Arc<PartitionDispatch<S>>,
    workers: SpinNoIrq<NormalWorkers>,
    notification: Arc<Waker>,
    freeze_policy: NormalFreezePolicy,
}

struct NormalWorkers {
    pool: WorkerPool,
    // Task-bound waits cannot be prepared before the corresponding task starts.
    // These weak-task wakers are installed before that slot becomes Available.
    wakers: KBox<[Option<Waker>]>,
    manager_waker: Option<Waker>,
    stop_requested: bool,
}

impl<S: Send + Sync + 'static> NormalService<S> {
    /// Prepares fixed tables before cyclic service construction. Flags constrain
    /// only these tables; existing Arc and notification allocations are unchanged.
    #[cfg(unittest)]
    pub(crate) fn new(
        cpu: LogicalCpuId,
        limit: NonZeroUsize,
        flags: AllocFlags,
    ) -> KResult<Arc<Self>> {
        Self::new_with_policy(cpu, limit, flags, NormalFreezePolicy::KeepRunning)
    }

    fn new_with_policy(
        cpu: LogicalCpuId,
        limit: NonZeroUsize,
        flags: AllocFlags,
        freeze_policy: NormalFreezePolicy,
    ) -> KResult<Arc<Self>> {
        assert!(kcpu_id_map::raw_cpu_id(cpu).is_some(), "invalid worker CPU");
        let pool = WorkerPool::new(limit, flags)?;
        let wakers = empty_slots(limit, flags)?;
        Ok(Arc::new_cyclic(|owner| Self {
            cpu,
            dispatch: PartitionDispatch::new(),
            workers: SpinNoIrq::new(NormalWorkers {
                pool,
                wakers,
                manager_waker: None,
                stop_requested: false,
            }),
            notification: wake::prepare(owner.clone()),
            freeze_policy,
        }))
    }
}

fn empty_slots<T>(limit: NonZeroUsize, flags: AllocFlags) -> KResult<KBox<[Option<T>]>> {
    let mut storage = KBox::<[Option<T>]>::new_uninit_slice_with_flags(limit.get(), flags)?;
    for slot in storage.iter_mut() {
        slot.write(None);
    }
    // SAFETY: every slot contains None, a valid Option<T>; no initialization can
    // fail or panic, and the unique owner transfers all initialized elements.
    Ok(unsafe { storage.assume_init() })
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
            // No source inspection, execution payload or worker lock survives
            // this boundary. Claimed callbacks finish before the next checkpoint.
            self.freeze_policy.checkpoint();
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
                self.freeze_policy.park(&mut wait);
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

unittest::path_mod_test!(normal_unittest, "../ut-tests/normal_unittest.rs");
