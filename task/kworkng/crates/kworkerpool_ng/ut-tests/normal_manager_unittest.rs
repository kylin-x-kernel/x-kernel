// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicitly driven lifecycle passes with real tasks; not autonomous manager tests.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::def_test;

use super::*;
use crate::{AcquiredExecution, SourceRemainder};

struct Probe {
    pending: AtomicUsize,
    entered: AtomicUsize,
    completed: AtomicUsize,
    hold: AtomicBool,
    manager_key: u64,
}

impl PartitionWorkSource for Arc<Probe> {
    type Execution = Arc<Probe>;

    fn acquire(&self) -> (Option<Self::Execution>, SourceRemainder) {
        match self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        {
            Ok(n) => (
                Some(self.clone()),
                if n > 1 {
                    SourceRemainder::More
                } else {
                    SourceRemainder::Empty
                },
            ),
            Err(_) => (None, SourceRemainder::Empty),
        }
    }
}

impl AcquiredExecution for Arc<Probe> {
    fn run(self) {
        assert_ne!(ktask::current().owner_key(), self.manager_key);
        assert!(ktask::is_sleepable_context());
        self.entered.fetch_add(1, Ordering::Release);
        while self.hold.load(Ordering::Acquire) {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        self.completed.fetch_add(1, Ordering::Release);
    }
}

fn until(mut ready: impl FnMut() -> bool) {
    // External QEMU timeout diagnoses hangs; progress has no local time threshold.
    while !ready() {
        ktask::yield_now();
    }
}

fn manager(limit: usize) -> WorkerManager<Arc<Probe>> {
    WorkerManager::new(
        khal::percpu::this_cpu_id(),
        NonZeroUsize::new(limit).unwrap(),
        TimeSpan::from_millis(10),
    )
}

fn pass(manager: &mut WorkerManager<Arc<Probe>>) {
    manager.run_pass(khal::time::monotonic_time());
}

fn all_parked(manager: &WorkerManager<Arc<Probe>>) -> bool {
    let workers = manager.service.workers.lock();
    (0..manager.tasks.len()).all(|id| {
        matches!(
            workers.pool.state(id),
            WorkerState::Empty | WorkerState::Parked { .. }
        )
    })
}

fn stop(manager: &mut WorkerManager<Arc<Probe>>) {
    until(|| manager.stop_if_idle());
    until(|| {
        pass(manager);
        manager.is_stopped()
    });
    assert_eq!(manager.service.workers.lock().pool.reserved_workers(), 0);
}

fn publish(
    manager: &WorkerManager<Arc<Probe>>,
    count: usize,
) -> (Arc<Probe>, Arc<crate::PartitionSource<Arc<Probe>>>) {
    publish_to(&manager.service, count, ktask::current().owner_key())
}

fn publish_to(
    service: &NormalService<Arc<Probe>>,
    count: usize,
    manager_key: u64,
) -> (Arc<Probe>, Arc<crate::PartitionSource<Arc<Probe>>>) {
    let probe = Arc::new(Probe {
        pending: AtomicUsize::new(count),
        entered: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        hold: AtomicBool::new(true),
        manager_key,
    });
    let source = service.dispatch.create_source(probe.clone());
    service.dispatch.notify(&source);
    service.notification_waker().wake_by_ref();
    (probe, source)
}

/// A sleeping occupied worker causes one bounded creation without exceeding H.
///
/// # Panics
/// Panics on overcommit, callbacks running in the manager, or incorrect stop/reap state.
#[def_test]
fn manager_creates_for_retained_demand_and_respects_capacity() {
    let mut manager = manager(2);
    pass(&mut manager);
    until(|| all_parked(&manager));
    let (probe, source) = publish(&manager, 3);
    until(|| probe.entered.load(Ordering::Acquire) == 1);
    pass(&mut manager);
    until(|| probe.entered.load(Ordering::Acquire) == 2);
    pass(&mut manager);
    let reserved = manager.service.workers.lock().pool.reserved_workers();
    let owned = manager.tasks.iter().filter(|task| task.is_some()).count();
    let refused_stop = !manager.stop_if_idle();
    probe.hold.store(false, Ordering::Release);
    until(|| probe.completed.load(Ordering::Acquire) == 3);
    manager.service.dispatch.detach(&source);
    stop(&mut manager);
    assert_eq!(reserved, 2);
    assert_eq!(owned, 2);
    assert!(refused_stop);
}

/// A one-worker service retains its baseline and never creates beyond its limit.
///
/// # Panics
/// Panics on a lost baseline, excess reservation or incorrect teardown.
#[def_test]
fn manager_single_worker_limit_preserves_baseline() {
    let mut manager = manager(1);
    pass(&mut manager);
    until(|| all_parked(&manager));
    assert!(
        manager
            .next_deadline(khal::time::monotonic_time())
            .is_none()
    );
    manager.run_pass(khal::time::monotonic_time() + TimeSpan::from_secs(1));
    let baseline = manager.service.workers.lock().pool.reserved_workers();
    let (probe, source) = publish(&manager, 2);
    until(|| probe.entered.load(Ordering::Acquire) == 1);
    pass(&mut manager);
    let saturated = manager.service.workers.lock().pool.reserved_workers();
    probe.hold.store(false, Ordering::Release);
    until(|| probe.completed.load(Ordering::Acquire) == 2);
    manager.service.dispatch.detach(&source);
    until(|| all_parked(&manager));
    assert!(
        manager
            .next_deadline(khal::time::monotonic_time())
            .is_none()
    );
    stop(&mut manager);
    assert_eq!(baseline, 1);
    assert_eq!(saturated, 1);
}

/// Idle retirement keeps capacity until a later pass observes actual task exit.
///
/// # Panics
/// Panics on early slot reuse, lost idle baseline or a retained task after reaping.
#[def_test]
fn manager_retires_excess_and_reaps_before_capacity_reuse() {
    let mut manager = manager(2);
    pass(&mut manager);
    until(|| all_parked(&manager));
    let (probe, source) = publish(&manager, 2);
    until(|| probe.entered.load(Ordering::Acquire) == 1);
    pass(&mut manager);
    until(|| probe.entered.load(Ordering::Acquire) == 2);
    let task_lifetimes: alloc::vec::Vec<_> = manager
        .tasks
        .iter()
        .map(|task| ktask::KtaskRef::downgrade(task.as_ref().unwrap()))
        .collect();
    let refused_occupied_stop = !manager.stop_if_idle();
    probe.hold.store(false, Ordering::Release);
    until(|| probe.completed.load(Ordering::Acquire) == 2 && all_parked(&manager));
    manager.service.dispatch.detach(&source);
    // Synthetic policy time avoids asserting any real idle duration.
    manager.run_pass(khal::time::monotonic_time() + TimeSpan::from_secs(1));
    let reserved_before_reap = manager.service.workers.lock().pool.reserved_workers();
    let owned_before_reap = manager.tasks.iter().filter(|task| task.is_some()).count();
    until(|| {
        pass(&mut manager);
        manager.service.workers.lock().pool.reserved_workers() == 1
    });
    let owned_after_reap = manager.tasks.iter().filter(|task| task.is_some()).count();
    // A cleared manager slot alone does not prove scheduler ownership ended.
    // Observe actual strong-owner release after scheduler switch cleanup.
    until(|| {
        task_lifetimes
            .iter()
            .filter(|task| task.strong_count() == 0)
            .count()
            == 1
    });
    let (next, next_source) = publish(&manager, 1);
    next.hold.store(false, Ordering::Release);
    until(|| next.completed.load(Ordering::Acquire) == 1);
    manager.service.dispatch.detach(&next_source);
    stop(&mut manager);
    until(|| task_lifetimes.iter().all(|task| task.strong_count() == 0));
    assert_eq!(reserved_before_reap, 2);
    assert_eq!(owned_before_reap, 2);
    assert_eq!(owned_after_reap, 1);
    assert!(refused_occupied_stop);
}

/// An unstarted service can stop repeatedly without creating a baseline afterwards.
///
/// # Panics
/// Panics if stopping is not idempotent or a later pass creates a new task.
#[def_test]
fn manager_stopped_service_never_recreates_baseline() {
    let mut manager = manager(1);
    assert!(manager.stop_if_idle());
    assert!(manager.stop_if_idle());
    pass(&mut manager);
    assert!(manager.is_stopped());
    assert_eq!(manager.service.workers.lock().pool.reserved_workers(), 0);
}

fn start_manager(manager: WorkerManager<Arc<Probe>>) -> ktask::KtaskRef {
    let cpu = manager.service.cpu;
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || manager.run(),
        "workng_manager_probe".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    task.set_cpumask(KCpuMask::one_shot_logical(cpu));
    ktask::activate_task(&task);
    task
}

/// One publication drives growth, and a timer later retires excess without another kick.
///
/// # Panics
/// Panics on excess capacity, missing teardown or callback execution in the manager.
#[def_test]
fn manager_autonomously_grows_and_retires_without_new_submission() {
    let manager = manager(2);
    let service = manager.service.clone();
    let task = start_manager(manager);
    until(|| {
        let workers = service.workers.lock();
        workers.pool.reserved_workers() == 1
            && matches!(workers.pool.state(0), WorkerState::Parked { .. })
    });
    let (probe, source) = publish_to(&service, 3, task.owner_key());
    until(|| probe.entered.load(Ordering::Acquire) == 2);
    let saturated = service.workers.lock().pool.reserved_workers();
    probe.hold.store(false, Ordering::Release);
    until(|| probe.completed.load(Ordering::Acquire) == 3);
    service.dispatch.detach(&source);
    // No pass, source notification or explicit timer dispatch drives retirement.
    until(|| service.workers.lock().pool.reserved_workers() == 1);
    service.request_stop();
    let exit = task.join();
    drop(task);
    let workers = service.workers.lock();
    assert_eq!(saturated, 2);
    assert_eq!(exit, 0);
    assert_eq!(workers.pool.reserved_workers(), 0);
    assert!(workers.manager_waker.is_none());
}

/// A provider stop request made before manager startup is retained without a wake handle.
///
/// # Panics
/// Panics if startup loses stop intent or leaves task resources published.
#[def_test]
fn manager_retains_stop_requested_before_startup() {
    let manager = manager(1);
    let service = manager.service.clone();
    service.request_stop();
    let task = start_manager(manager);
    assert_eq!(task.join(), 0);
    drop(task);
    let workers = service.workers.lock();
    assert_eq!(workers.pool.reserved_workers(), 0);
    assert!(workers.manager_waker.is_none());
}

/// Demand queued before the manager installs its waker needs no replacement notification.
///
/// # Panics
/// Panics on incorrect execution count or incomplete provider teardown.
#[def_test]
fn manager_consumes_demand_published_before_startup() {
    let manager = manager(1);
    let service = manager.service.clone();
    let (probe, source) = publish(&manager, 1);
    probe.hold.store(false, Ordering::Release);
    let task = start_manager(manager);
    until(|| probe.completed.load(Ordering::Acquire) == 1);
    service.dispatch.detach(&source);
    service.request_stop();
    assert_eq!(task.join(), 0);
    drop(task);
    assert_eq!(service.workers.lock().pool.reserved_workers(), 0);
}
