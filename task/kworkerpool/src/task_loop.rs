// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    ActionBatch, KtaskWorkerPool, ParkDecision, PoolEntry, PoolId, RunnableCandidateResult,
    RunnableClaim, RunnableClaimer, WorkerExecutionToken, WorkerId, WorkerRuntime,
    encode_task_context,
};

/// ktask-backed worker task body for one core worker-pool instance.
///
/// The loop owns no product policy. It parks, claims, runs, and finishes opaque
/// entries from a provided pool slot; runtime code supplies wait/wake handling
/// and callback execution through [`WorkerRuntime`].
pub struct WorkerTask<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>>,
{
    pool: &'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>,
    worker: WorkerId,
    runtime: R,
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize> WorkerTask<R, MAX_WORKERS, ENTRY_CAP>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>>,
{
    /// Creates one ktask-backed worker task body.
    pub const fn new(
        pool: &'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>,
        worker: WorkerId,
        runtime: R,
    ) -> Self {
        Self {
            pool,
            worker,
            runtime,
        }
    }

    /// Runs until the pool asks this worker to exit.
    pub fn run(mut self) {
        loop {
            let now = self.runtime.now();
            let Ok((decision, actions)) = self.pool.lock().worker_ready_to_park(self.worker, now)
            else {
                self.runtime.wait_for_worker_work(self.pool, self.worker);
                continue;
            };
            self.runtime.handle_worker_actions(self.pool, actions);
            match decision {
                ParkDecision::Run => {}
                ParkDecision::Wait => {
                    self.runtime.wait_for_worker_work(self.pool, self.worker);
                    continue;
                }
                ParkDecision::Exit => {
                    let _ = self.pool.lock().worker_exit_complete(self.worker);
                    return;
                }
            }

            while self.run_one_runnable() {}
        }
    }

    fn run_one_runnable(&mut self) -> bool {
        let (token, work) = loop {
            let now = self.runtime.now();
            let candidate = self
                .pool
                .lock()
                .prepare_runnable_candidate(self.worker, now);
            let Ok(candidate) = candidate else {
                return false;
            };
            match candidate {
                RunnableCandidateResult::Empty(actions) => {
                    self.runtime.handle_worker_actions(self.pool, actions);
                    return false;
                }
                RunnableCandidateResult::Candidate(candidate) => {
                    self.runtime
                        .handle_worker_actions(self.pool, candidate.actions);
                    let token = candidate.token;
                    match self.runtime.claim(self.worker, token, candidate.entry) {
                        RunnableClaim::Run(work) => {
                            let actions = self.pool.lock().commit_runnable_candidate(
                                self.worker,
                                token,
                                self.runtime.now(),
                            );
                            let Ok(actions) = actions else {
                                warn!("worker-pool claim commit failed after external claim");
                                return false;
                            };
                            self.runtime.handle_worker_actions(self.pool, actions);
                            break (token, work);
                        }
                        RunnableClaim::Stale(stale) => {
                            self.runtime.record_stale(stale);
                            let discarded = self.pool.lock().discard_runnable_candidate(
                                self.worker,
                                token,
                                self.runtime.now(),
                            );
                            let Ok(discarded) = discarded else {
                                return false;
                            };
                            self.runtime
                                .handle_worker_actions(self.pool, discarded.actions);
                            if discarded.should_retry {
                                continue;
                            }
                            return false;
                        }
                    }
                }
            }
        };

        let pool_id = self.pool.lock().id();
        let _current = CurrentWorkerPoolExecutionGuard::enter(pool_id, self.worker, token);
        self.runtime
            .run_claimed_work(self.pool, self.worker, token, work);

        let actions = self
            .pool
            .lock()
            .worker_finished(self.worker, token, self.runtime.now());
        if let Ok(actions) = actions {
            self.runtime.handle_worker_actions(self.pool, actions);
        }
        true
    }
}

/// Installs worker-pool execution identity on the current ktask.
pub struct CurrentWorkerPoolExecutionGuard {
    context: ktask::TaskExecutionContext,
    previous: Option<ktask::TaskExecutionContext>,
}

impl CurrentWorkerPoolExecutionGuard {
    /// Enters a worker-pool execution context for scheduler accounting.
    pub fn enter(pool: PoolId, worker: WorkerId, token: WorkerExecutionToken) -> Self {
        let context = encode_task_context(pool, worker, token);
        let previous = ktask::set_current_execution_context(context);
        if previous.is_some() {
            warn!("nested worker-pool execution context entered");
        }
        ktask::refresh_current_execution_tick();
        Self { context, previous }
    }
}

impl Drop for CurrentWorkerPoolExecutionGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            let replaced = ktask::set_current_execution_context(previous);
            if replaced != Some(self.context) {
                warn!("worker-pool execution context changed during callback");
            }
        } else if !ktask::clear_current_execution_context(self.context) {
            warn!("worker-pool execution context changed during callback");
        }
        ktask::refresh_current_execution_tick();
    }
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize> RunnableClaimer
    for WorkerTask<R, MAX_WORKERS, ENTRY_CAP>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>>,
{
    type Accepted = R::Accepted;
    type Stale = R::Stale;

    fn claim(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        entry: PoolEntry,
    ) -> RunnableClaim<Self::Accepted, Self::Stale> {
        self.runtime.claim(worker, token, entry)
    }

    fn record_stale(&mut self, stale: Self::Stale) {
        self.runtime.record_stale(stale)
    }
}

#[allow(dead_code)]
fn _assert_action_batch_is_runtime_boundary(_: ActionBatch) {}
