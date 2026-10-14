// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real task wait probes. A state-only baseline isolates one worker's runtime;
//! these are not aggregate manager growth/retirement or concrete Work tests.

use alloc::{sync::Arc, task::Wake};
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kcpu_id_map::{KCpuMask, KCpuMaskExt};
use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;
use crate::SourceRemainder;

struct Probe {
    pending: AtomicUsize,
    entered: AtomicUsize,
    completed: AtomicUsize,
    sleep_in_callback: bool,
    hold_first: AtomicBool,
    last_cpu: AtomicUsize,
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
        assert!(ktask::is_sleepable_context());
        self.last_cpu
            .store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
        let first = self.entered.fetch_add(1, Ordering::AcqRel) == 0;
        if first {
            while self.hold_first.load(Ordering::Acquire) {
                ktask::sleep(TimeSpan::from_millis(1));
            }
        }
        if self.sleep_in_callback {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        self.completed.fetch_add(1, Ordering::Release);
    }
}

fn prepare() -> (Arc<NormalService<Arc<Probe>>>, usize) {
    prepare_on(khal::percpu::this_cpu_id())
}

fn prepare_on(cpu: LogicalCpuId) -> (Arc<NormalService<Arc<Probe>>>, usize) {
    let service = NormalService::new(cpu, NonZeroUsize::new(2).unwrap());
    let worker = {
        let mut workers = service.workers.lock();
        // Keep retirement policy out of this isolated one-thread wait probe.
        let baseline = workers.pool.reserve_worker().unwrap();
        workers.pool.worker_prepared(baseline);
        workers.pool.worker_started(baseline);
        let worker = workers.pool.reserve_worker().unwrap();
        workers.pool.worker_prepared(worker);
        worker
    };
    (service, worker)
}

fn start(
    service: &Arc<NormalService<Arc<Probe>>>,
    run: impl FnOnce() + Send + 'static,
) -> ktask::KtaskRef {
    start_on(service.cpu, run)
}

fn start_on(cpu: LogicalCpuId, run: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        run,
        "workng_normal_probe".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    task.set_cpumask(KCpuMask::one_shot_logical(cpu));
    ktask::activate_task(&task);
    task
}

fn until(mut ready: impl FnMut() -> bool) {
    // The bounded QEMU runner diagnoses lack of progress; no host-speed threshold
    // is part of this protocol assertion.
    while !ready() {
        ktask::yield_now();
    }
}

fn stop(service: &NormalService<Arc<Probe>>, worker: usize, task: ktask::KtaskRef) {
    until(|| {
        matches!(
            service.workers.lock().pool.state(worker),
            WorkerState::Parked { .. }
        )
    });
    let wake = {
        let mut workers = service.workers.lock();
        // Use a synthetic policy sample: no wall-clock idle-policy claim here.
        let now = khal::time::monotonic_time() + TimeSpan::from_secs(1);
        assert!(
            workers
                .pool
                .request_retirement(worker, now, TimeSpan::from_nanos(1))
        );
        workers.wakers[worker].as_ref().unwrap().clone()
    };
    wake.wake();
    assert_eq!(task.join(), 0);
    drop(task);
    service.worker_exited(worker);
}

fn probe(pending: usize, sleep_in_callback: bool) -> Arc<Probe> {
    Arc::new(Probe {
        pending: AtomicUsize::new(pending),
        entered: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        sleep_in_callback,
        hold_first: AtomicBool::new(false),
        last_cpu: AtomicUsize::new(usize::MAX),
    })
}

/// Demand published before task startup is consumed without a later submission.
///
/// # Panics
/// Panics on wrong context, incorrect execution counts or invalid lifecycle transitions.
#[def_test]
fn normal_worker_consumes_prestart_demand_and_sleeping_callbacks() {
    let (service, worker) = prepare();
    let probe = probe(3, true);
    let source = service.dispatch.create_source(probe.clone());
    service.dispatch.notify(&source);
    service.notification_waker().wake_by_ref();
    let running = service.clone();
    let task = start(&service, move || running.run_worker(worker));
    until(|| probe.completed.load(Ordering::Acquire) == 3);
    stop(&service, worker, task);
    service.dispatch.detach(&source);
    assert_eq!(probe.pending.load(Ordering::Acquire), 0);
}

/// A wake delivered between Parked publication and task park remains latched.
///
/// # Panics
/// Panics on duplicated execution or invalid lifecycle transitions.
#[def_test]
fn normal_worker_wake_before_actual_park_is_not_lost() {
    let (service, worker) = prepare();
    let probe = probe(1, false);
    let source = service.dispatch.create_source(probe.clone());
    let running = service.clone();
    let producer = service.clone();
    let published = source.clone();
    let task = start(&service, move || {
        let mut first = true;
        running.run_worker_before_park(worker, || {
            if first {
                first = false;
                producer.dispatch.notify(&published);
                producer.notification_waker().wake_by_ref();
            }
        });
    });
    until(|| probe.completed.load(Ordering::Acquire) == 1);
    stop(&service, worker, task);
    service.dispatch.detach(&source);
    assert_eq!(probe.completed.load(Ordering::Acquire), 1);
}

/// Spurious task wake does not change idle age; later demand still wakes the task.
///
/// # Panics
/// Panics on changed idle age or invalid lifecycle transitions.
#[def_test]
fn normal_worker_rechecks_after_spurious_wake() {
    let (service, worker) = prepare();
    let parks = Arc::new(AtomicUsize::new(0));
    let observed = parks.clone();
    let running = service.clone();
    let task = start(&service, move || {
        running.run_worker_before_park(worker, || {
            observed.fetch_add(1, Ordering::Release);
        });
    });
    until(|| parks.load(Ordering::Acquire) >= 1);
    let (phase, wake) = {
        let workers = service.workers.lock();
        (
            workers.pool.state(worker),
            workers.wakers[worker].as_ref().unwrap().clone(),
        )
    };
    wake.wake_by_ref();
    until(|| parks.load(Ordering::Acquire) >= 2);
    let unchanged = service.workers.lock().pool.state(worker) == phase;
    let probe = probe(1, false);
    let source = service.dispatch.create_source(probe.clone());
    service.dispatch.notify(&source);
    service.notification_waker().wake_by_ref();
    until(|| probe.completed.load(Ordering::Acquire) == 1);
    stop(&service, worker, task);
    service.dispatch.detach(&source);
    assert!(unchanged);
}

/// Retained demand wakes a second task while the first callback really sleeps.
///
/// # Panics
/// Panics on sleeping-worker reuse, early capacity release or invalid lifecycle transitions.
#[def_test]
fn normal_worker_retained_demand_progresses_while_callback_sleeps() {
    let cpu = khal::percpu::this_cpu_id();
    let service = NormalService::new(cpu, NonZeroUsize::new(3).unwrap());
    let ids = {
        let mut workers = service.workers.lock();
        let baseline = workers.pool.reserve_worker().unwrap();
        workers.pool.worker_prepared(baseline);
        workers.pool.worker_started(baseline);
        [0; 2].map(|_| {
            let id = workers.pool.reserve_worker().unwrap();
            workers.pool.worker_prepared(id);
            id
        })
    };
    let tasks = ids.map(|id| {
        let running = service.clone();
        start(&service, move || running.run_worker(id))
    });
    until(|| {
        let workers = service.workers.lock();
        ids.iter()
            .all(|id| matches!(workers.pool.state(*id), WorkerState::Parked { .. }))
    });
    let probe = probe(2, false);
    probe.hold_first.store(true, Ordering::Release);
    let source = service.dispatch.create_source(probe.clone());
    service.dispatch.notify(&source);
    service.notification_waker().wake_by_ref();
    until(|| probe.completed.load(Ordering::Acquire) == 1);
    // completed is published inside run(), before the worker retires Occupied.
    // Wait for the second worker to park while the first callback remains held.
    until(|| {
        let workers = service.workers.lock();
        ids.iter()
            .any(|id| matches!(workers.pool.state(*id), WorkerState::Parked { .. }))
    });
    let (occupied, reserved) = {
        let workers = service.workers.lock();
        (
            ids.iter()
                .filter(|id| workers.pool.state(**id) == WorkerState::Occupied)
                .count(),
            workers.pool.reserved_workers(),
        )
    };
    probe.hold_first.store(false, Ordering::Release);
    until(|| probe.completed.load(Ordering::Acquire) == 2);
    for (id, task) in ids.into_iter().zip(tasks) {
        stop(&service, id, task);
    }
    service.dispatch.detach(&source);
    assert_eq!(occupied, 1);
    assert_eq!(reserved, 3);
    assert_eq!(service.workers.lock().pool.reserved_workers(), 1);
}

/// An IRQ-masked CPU0 publisher wakes a parked CPU1 worker through prepared storage.
///
/// # Panics
/// Panics on wrong callback CPU/context, duplicate execution or invalid lifecycle.
#[def_test]
fn normal_prepared_notification_wakes_remote_worker_with_irqs_masked() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let (service, worker) = prepare_on(LogicalCpuId::new(1));
    let running = service.clone();
    let task = start(&service, move || running.run_worker(worker));
    until(|| {
        matches!(
            service.workers.lock().pool.state(worker),
            WorkerState::Parked { .. }
        )
    });
    let probe = probe(1, false);
    let source = service.dispatch.create_source(probe.clone());
    let dispatch = service.dispatch.clone();
    let published = source.clone();
    let notification = service.notification_waker();
    let publisher = start_on(LogicalCpuId::new(0), move || {
        let _irq = kspin::NoPreemptIrqSave::new();
        dispatch.notify(&published);
        notification.wake_by_ref();
    });
    let publisher_exit = publisher.join();
    drop(publisher);
    until(|| probe.completed.load(Ordering::Acquire) == 1);
    stop(&service, worker, task);
    service.dispatch.detach(&source);
    assert_eq!(publisher_exit, 0);
    assert_eq!(probe.last_cpu.load(Ordering::Acquire), 1);
    assert_eq!(probe.completed.load(Ordering::Acquire), 1);
}

/// Notification storage neither retains an idle service nor accesses it after release.
///
/// # Panics
/// Panics if a notification introduces a strong service ownership cycle.
#[def_test]
fn normal_notification_can_outlive_quiescent_service() {
    let service = NormalService::<Arc<Probe>>::new(
        khal::percpu::this_cpu_id(),
        NonZeroUsize::new(1).unwrap(),
    );
    let weak = Arc::downgrade(&service);
    let notification = service.notification_waker();
    notification.wake_by_ref();
    drop(service);
    assert!(weak.upgrade().is_none());
    let _irq = kspin::NoPreemptIrqSave::new();
    notification.wake_by_ref();
}

struct ManagerWakeCount(AtomicUsize);

impl Wake for ManagerWakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A pure state probe distinguishes growable demand from a saturated manager self-kick.
///
/// # Panics
/// Panics if a full service wakes management again or growable demand is not notified.
#[def_test]
fn normal_growth_notification_does_not_spin_when_capacity_is_full() {
    for limit in [1, 2] {
        let service = NormalService::new(
            khal::percpu::this_cpu_id(),
            NonZeroUsize::new(limit).unwrap(),
        );
        let count = Arc::new(ManagerWakeCount(AtomicUsize::new(0)));
        let source = service.dispatch.create_source(probe(1, false));
        {
            let mut workers = service.workers.lock();
            let worker = workers.pool.reserve_worker().unwrap();
            workers.pool.worker_prepared(worker);
            workers.pool.worker_started(worker);
            assert!(workers.pool.begin_selection(worker));
            workers.pool.finish_selection(worker, true);
            workers.manager_waker = Some(Waker::from(count.clone()));
        }
        service.dispatch.notify(&source);
        service.kick();
        service.dispatch.detach(&source);
        assert_eq!(count.0.load(Ordering::Relaxed), usize::from(limit > 1));
    }
}
