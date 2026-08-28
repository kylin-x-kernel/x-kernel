// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Worker-pool manager loop and target traits.

use ktime_types::MonotonicInstant;

use crate::{ActionBatch, WorkerId, action::ManagementAction};

/// Maximum lifecycle actions a manager performs for one pool in one pass.
pub const MANAGEMENT_ACTION_BUDGET_PER_PASS: usize = 2;

/// Pool target operations required by the worker-pool manager runtime.
///
/// The pool state and queue policy stay behind the pool implementation. The
/// manager only asks whether slow-path lifecycle work is ready, reserves one
/// action, and completes runtime-side spawn attempts.
pub trait ManagerTarget {
    /// Returns whether the manager should run now.
    fn manager_should_run(&self, now: MonotonicInstant) -> bool;

    /// Returns the next time a sleeping manager should wake without an
    /// external event.
    fn next_management_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant>;

    /// Reserves the next lifecycle action for runtime execution.
    fn next_management_action(&self, now: MonotonicInstant) -> Option<ManagementAction>;

    /// Completes a runtime worker spawn attempt and returns any fast-path
    /// actions produced by the completion transition.
    fn complete_worker_spawn(
        &self,
        worker: WorkerId,
        success: bool,
        now: MonotonicInstant,
    ) -> ActionBatch;
}

/// Runtime operations used by the generic worker-pool manager runtime.
///
/// The runtime owns concrete execution resources: kernel tasks, wait sources,
/// CPU masks, and sleep/backoff primitives. It does not choose worker-pool
/// policy; that remains in [`ManagerTarget::next_management_action`].
pub trait ManagerRuntime<P>
where
    P: ManagerTarget,
{
    /// Returns the current time used for pool lifecycle decisions.
    fn now(&self) -> MonotonicInstant;

    /// Blocks until at least one pool can make manager progress.
    fn wait_for_manager_work<const N: usize>(&mut self, pools: &[P; N]);

    /// Spawns the runtime worker for a reserved worker slot.
    fn spawn_worker(&mut self, pool: &P, worker: WorkerId) -> bool;

    /// Wakes a worker that has been marked for retirement.
    fn wake_retiring_worker(&mut self, pool: &P, worker: WorkerId);

    /// Applies fast-path actions returned by manager-owned state transitions.
    fn handle_pool_actions(&mut self, pool: &P, actions: ActionBatch);

    /// Runs runtime-side failure backoff after a failed spawn.
    fn after_spawn_failure(&mut self, _pool: &P, _worker: WorkerId) {}
}

/// Generic worker-pool manager task body.
///
/// This object owns the manager-side pool set and the runtime hooks used by
/// the loop.
pub struct ManagerTask<P, R, const N: usize>
where
    P: ManagerTarget,
    R: ManagerRuntime<P>,
{
    pools: [P; N],
    runtime: R,
}

impl<P, R, const N: usize> ManagerTask<P, R, N>
where
    P: ManagerTarget,
    R: ManagerRuntime<P>,
{
    /// Creates a manager task body for one CPU-local pool set.
    pub const fn new(pools: [P; N], runtime: R) -> Self {
        Self { pools, runtime }
    }

    /// Runs the manager task forever.
    pub fn run(mut self) -> ! {
        loop {
            self.runtime.wait_for_manager_work(&self.pools);
            run_worker_pool_manager_set_pass(&self.pools, &mut self.runtime);
        }
    }
}

/// Processes one bounded manager pass for `pool`.
pub fn run_worker_pool_manager_pass<P, R>(pool: &P, runtime: &mut R)
where
    P: ManagerTarget,
    R: ManagerRuntime<P>,
{
    run_worker_pool_manager_pool_pass(pool, runtime);
}

/// Processes one bounded per-CPU manager pass across `pools`.
pub fn run_worker_pool_manager_set_pass<P, R, const N: usize>(pools: &[P; N], runtime: &mut R)
where
    P: ManagerTarget,
    R: ManagerRuntime<P>,
{
    for pool in pools {
        run_worker_pool_manager_pool_pass(pool, runtime);
    }
}

fn run_worker_pool_manager_pool_pass<P, R>(pool: &P, runtime: &mut R)
where
    P: ManagerTarget,
    R: ManagerRuntime<P>,
{
    let now = runtime.now();
    if !pool.manager_should_run(now) {
        return;
    }

    for _ in 0..MANAGEMENT_ACTION_BUDGET_PER_PASS {
        let now = runtime.now();
        let Some(action) = pool.next_management_action(now) else {
            return;
        };
        match action {
            ManagementAction::SpawnWorker { worker, .. } => {
                let success = runtime.spawn_worker(pool, worker);
                let actions = pool.complete_worker_spawn(worker, success, runtime.now());
                runtime.handle_pool_actions(pool, actions);
                if !success {
                    runtime.after_spawn_failure(pool, worker);
                    return;
                }
            }
            ManagementAction::RetireWorker { worker, .. } => {
                runtime.wake_retiring_worker(pool, worker);
            }
        }
    }
}
