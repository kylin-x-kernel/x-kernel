// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use crate::{
    ActionBatch, EntryKey, EntryOwner, EntryPayload, EntrySource, ImmediateAction,
    ManagementAction, ManagementComplete, ManagerRuntime, ManagerTarget, PoolEntry, PoolId,
    PoolKind, RunnableCandidateResult, WorkerId, WorkerPool, WorkerPoolPolicy,
    WorkerPoolPolicyConfig, WorkerState, run_worker_pool_manager_pass,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestThreadRef(usize);

fn now(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

const fn policy(manager_managed: bool) -> WorkerPoolPolicy {
    WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
        min_workers: 1,
        initial_workers: 1,
        max_workers: 4,
        idle_retire_after: Some(TimeSpan::from_millis(10)),
        create_retry_delay: TimeSpan::from_millis(1),
        cpu_intensive_threshold: TimeSpan::from_millis(10),
        manager_managed,
        dynamic_create: true,
        idle_retire: true,
    })
}

const fn pool_id() -> PoolId {
    PoolId::new(PoolKind::new(1), LogicalCpuId::new(0))
}

fn entry(key: usize) -> PoolEntry {
    PoolEntry::new(
        EntrySource::new(17),
        EntryOwner::new(7),
        EntryKey::new(key),
        EntryPayload::new(key),
    )
}

#[def_test]
fn runnable_enqueue_wakes_idle_worker() {
    let mut pool: WorkerPool<TestThreadRef, 4, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );

    let actions = pool.enqueue_runnable(entry(1), now(1)).unwrap();
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(0)
        )
    }));
}

#[def_test]
fn preparing_worker_remains_wait_ready_and_can_be_rewoken() {
    let mut pool: WorkerPool<TestThreadRef, 4, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );

    assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Preparing)
    );
    assert!(pool.worker_wait_ready(WorkerId::new(0)));

    let actions = pool.enqueue_runnable(entry(2), now(2)).unwrap();
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(0)
        )
    }));
}

#[def_test]
fn unmanaged_pool_raises_bottom_half_instead_of_worker_wake() {
    let mut pool: WorkerPool<TestThreadRef, 1, 4> = WorkerPool::new(pool_id(), policy(false));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );

    let actions = pool.enqueue_runnable(entry(1), now(1)).unwrap();
    assert!(
        actions
            .immediate()
            .any(|action| matches!(action, ImmediateAction::RaiseBottomHalf { .. }))
    );
}

#[def_test]
fn deferred_entries_promote_by_owner_fifo() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    let owner = EntryOwner::new(11);
    let other = EntryOwner::new(12);
    assert!(
        pool.enqueue_deferred(PoolEntry::new(
            EntrySource::new(21),
            owner,
            EntryKey::new(1),
            EntryPayload::new(1)
        ))
        .is_ok()
    );
    assert!(
        pool.enqueue_deferred(PoolEntry::new(
            EntrySource::new(22),
            other,
            EntryKey::new(2),
            EntryPayload::new(2)
        ))
        .is_ok()
    );
    assert!(
        pool.enqueue_deferred(PoolEntry::new(
            EntrySource::new(23),
            owner,
            EntryKey::new(3),
            EntryPayload::new(3)
        ))
        .is_ok()
    );

    let (promoted, _) = pool.promote_deferred(owner, 2, now(1));
    assert_eq!(promoted, 2);
    assert_eq!(pool.runnable_len_for_owner(owner), 2);
    assert_eq!(pool.queued_len_for_owner(other), 1);
}

#[def_test]
fn prepare_commit_finish_tracks_concurrency() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );
    assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());

    let candidate = match pool
        .prepare_runnable_candidate(WorkerId::new(0), now(2))
        .unwrap()
    {
        RunnableCandidateResult::Candidate(candidate) => candidate,
        RunnableCandidateResult::Empty(_) => panic!("expected runnable candidate"),
    };
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Claiming)
    );
    assert!(
        pool.commit_runnable_candidate(WorkerId::new(0), candidate.token, now(3))
            .is_ok()
    );
    assert_eq!(pool.nr_concurrency(), 1);
    assert!(
        pool.worker_finished(WorkerId::new(0), candidate.token, now(4))
            .is_ok()
    );
    assert_eq!(pool.nr_concurrency(), 0);
}

#[def_test]
fn blocked_worker_releases_concurrency_and_wakes_another_worker() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );
    assert!(
        pool.install_worker(WorkerId::new(1), TestThreadRef(1))
            .is_ok()
    );
    assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());

    let candidate = match pool
        .prepare_runnable_candidate(WorkerId::new(0), now(2))
        .unwrap()
    {
        RunnableCandidateResult::Candidate(candidate) => candidate,
        RunnableCandidateResult::Empty(_) => panic!("expected runnable candidate"),
    };
    assert!(
        pool.commit_runnable_candidate(WorkerId::new(0), candidate.token, now(3))
            .is_ok()
    );
    assert_eq!(pool.nr_concurrency(), 1);

    assert!(pool.enqueue_runnable(entry(2), now(4)).is_ok());
    let actions = pool
        .worker_blocked(WorkerId::new(0), candidate.token, now(5))
        .unwrap();

    assert_eq!(pool.nr_concurrency(), 0);
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Sleeping)
    );
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(1)
        )
    }));
}

#[def_test]
fn cpu_intensive_worker_releases_concurrency_and_wakes_another_worker() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );
    assert!(
        pool.install_worker(WorkerId::new(1), TestThreadRef(1))
            .is_ok()
    );
    assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());

    let candidate = match pool
        .prepare_runnable_candidate(WorkerId::new(0), now(2))
        .unwrap()
    {
        RunnableCandidateResult::Candidate(candidate) => candidate,
        RunnableCandidateResult::Empty(_) => panic!("expected runnable candidate"),
    };
    assert!(
        pool.commit_runnable_candidate(WorkerId::new(0), candidate.token, now(3))
            .is_ok()
    );
    assert!(pool.enqueue_runnable(entry(2), now(4)).is_ok());

    let actions = pool
        .worker_tick(WorkerId::new(0), candidate.token, now(10_000_003))
        .unwrap();

    assert_eq!(pool.nr_concurrency(), 0);
    assert!(pool.worker_is_cpu_intensive(WorkerId::new(0)));
    assert_eq!(
        pool.worker_state(WorkerId::new(1)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(1)
        )
    }));
}

#[def_test]
fn retire_requested_worker_is_reused_before_exit() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(
        pool.install_worker(WorkerId::new(0), TestThreadRef(0))
            .is_ok()
    );
    assert!(
        pool.install_worker(WorkerId::new(1), TestThreadRef(1))
            .is_ok()
    );
    assert!(pool.worker_ready_to_park(WorkerId::new(0), now(1)).is_ok());

    assert!(matches!(
        pool.next_management_action(now(10_000_001)),
        Some(ManagementAction::RetireWorker {
            worker,
            ..
        }) if worker == WorkerId::new(0)
    ));
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::RetireRequested)
    );

    let actions = pool.enqueue_runnable(entry(1), now(10_000_002)).unwrap();
    assert_eq!(
        pool.worker_state(WorkerId::new(0)),
        Some(WorkerState::Preparing)
    );
    assert!(actions.immediate().any(|action| {
        matches!(
            action,
            ImmediateAction::WakeWorker {
                worker,
                ..
            } if worker == WorkerId::new(0)
        )
    }));
}

#[def_test]
fn failed_spawn_uses_retry_deadline_before_next_spawn() {
    let mut pool: WorkerPool<TestThreadRef, 2, 8> = WorkerPool::new(pool_id(), policy(true));
    assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());

    assert!(matches!(
        pool.next_management_action(now(2)),
        Some(ManagementAction::SpawnWorker {
            worker,
            ..
        }) if worker == WorkerId::new(0)
    ));
    assert!(
        pool.spawn_complete(WorkerId::new(0), ManagementComplete::SpawnFailed, now(3))
            .is_ok()
    );

    assert!(!pool.manager_should_run(now(999_999)));
    assert!(pool.manager_should_run(now(1_000_003)));
    assert!(matches!(
        pool.next_management_action(now(1_000_003)),
        Some(ManagementAction::SpawnWorker {
            worker,
            ..
        }) if worker == WorkerId::new(0)
    ));
}

#[derive(Clone, Copy)]
struct TestManagerPool(&'static SpinNoIrq<WorkerPool<TestThreadRef, 2, 8>>);

impl ManagerTarget for TestManagerPool {
    fn manager_should_run(&self, now: MonotonicInstant) -> bool {
        self.0.lock().manager_should_run(now)
    }

    fn next_management_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant> {
        self.0.lock().next_management_deadline(now)
    }

    fn next_management_action(&self, now: MonotonicInstant) -> Option<ManagementAction> {
        self.0.lock().next_management_action(now)
    }

    fn complete_worker_spawn(
        &self,
        worker: WorkerId,
        success: bool,
        now: MonotonicInstant,
    ) -> ActionBatch {
        let result = if success {
            ManagementComplete::Spawned
        } else {
            ManagementComplete::SpawnFailed
        };
        self.0
            .lock()
            .spawn_complete(worker, result, now)
            .unwrap_or_default()
    }
}

struct ManagerTestRuntime {
    now: MonotonicInstant,
    spawned: usize,
}

impl ManagerRuntime<TestManagerPool> for ManagerTestRuntime {
    fn now(&self) -> MonotonicInstant {
        self.now
    }

    fn wait_for_manager_work<const N: usize>(&mut self, _pools: &[TestManagerPool; N]) {}

    fn spawn_worker(&mut self, _pool: &TestManagerPool, _worker: WorkerId) -> bool {
        self.spawned += 1;
        true
    }

    fn wake_retiring_worker(&mut self, _pool: &TestManagerPool, _worker: WorkerId) {}

    fn handle_pool_actions(&mut self, _pool: &TestManagerPool, _actions: ActionBatch) {}
}

#[def_test]
fn manager_pass_spawns_reserved_worker() {
    static POOL: SpinNoIrq<WorkerPool<TestThreadRef, 2, 8>> =
        SpinNoIrq::new(WorkerPool::new(pool_id(), policy(true)));
    {
        let mut pool = POOL.lock();
        assert!(pool.enqueue_runnable(entry(1), now(1)).is_ok());
    }
    let mut runtime = ManagerTestRuntime {
        now: now(2),
        spawned: 0,
    };
    let pool = TestManagerPool(&POOL);

    run_worker_pool_manager_pass(&pool, &mut runtime);
    assert_eq!(runtime.spawned, 1);
    assert_eq!(
        POOL.lock().worker_state(WorkerId::new(0)),
        Some(WorkerState::Creating)
    );
}
