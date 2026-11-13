// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real scheduler load/put pairing, with event-driven progress checks.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::Completion;
use kcpu_id_map::LogicalCpuId;
use kiface_task_api::{ScheduleOutReason, TaskScheduleObserver};
use unittest::{assert, assert_eq, def_test};

const NO_CPU: usize = usize::MAX;

struct Probe {
    cpu: AtomicUsize,
    loads: AtomicUsize,
    puts: AtomicUsize,
    visited: AtomicUsize,
    has_blocked: AtomicBool,
    has_exited: AtomicBool,
    is_invalid: AtomicBool,
}

impl Probe {
    fn new() -> Self {
        Self {
            cpu: AtomicUsize::new(NO_CPU),
            loads: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            visited: AtomicUsize::new(0),
            has_blocked: AtomicBool::new(false),
            has_exited: AtomicBool::new(false),
            is_invalid: AtomicBool::new(false),
        }
    }

    fn check_context(&self, cpu: LogicalCpuId) {
        if karch::local_irq_enabled() || khal::percpu::this_cpu_id() != cpu {
            self.is_invalid.store(true, Ordering::Relaxed);
        }
    }
}

// SAFETY: callbacks only sample the CPU/IRQ state and update atomics. They do
// not allocate, take locks, enable IRQs, wait, or enter the scheduler.
unsafe impl TaskScheduleObserver for Probe {
    unsafe fn schedule_in(&self, cpu: LogicalCpuId) {
        self.check_context(cpu);
        if self.cpu.swap(cpu.as_usize(), Ordering::Relaxed) != NO_CPU
            || self.has_exited.load(Ordering::Relaxed)
        {
            self.is_invalid.store(true, Ordering::Relaxed);
        }
        self.visited
            .fetch_or(1 << cpu.as_usize(), Ordering::Relaxed);
        self.loads.fetch_add(1, Ordering::Relaxed);
    }

    unsafe fn schedule_out(&self, cpu: LogicalCpuId, reason: ScheduleOutReason) {
        self.check_context(cpu);
        if self.cpu.swap(NO_CPU, Ordering::Relaxed) != cpu.as_usize() {
            self.is_invalid.store(true, Ordering::Relaxed);
        }
        self.puts.fetch_add(1, Ordering::Relaxed);
        match reason {
            ScheduleOutReason::Blocked => self.has_blocked.store(true, Ordering::Release),
            ScheduleOutReason::Exited => self.has_exited.store(true, Ordering::Release),
            ScheduleOutReason::Runnable => {}
        }
    }
}

#[def_test]
fn unpublished_observer_does_not_acquire_cpu_state() {
    let probe = Arc::new(Probe::new());
    let task = crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new(|| {}).unwrap(), "observer-unused".into(), 0x4000,
    ).with_schedule_observer(probe.clone());
    drop(task);
    assert_eq!(probe.loads.load(Ordering::Relaxed), 0);
    assert_eq!(probe.puts.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&probe), 1);
}

#[def_test]
fn first_entry_block_resume_and_terminal_exit_are_paired() {
    let probe = Arc::new(Probe::new());
    let release = Arc::new(Completion::new());
    let worker_probe = probe.clone();
    let worker_release = release.clone();
    let task = crate::spawn_task(
        crate::TaskInner::new_pidless_kthread_with_entry(
            crate::PreparedTaskEntry::new(move || {
                {
                    let _irq = kspin::IrqSave::new();
                    core::assert_eq!(
                        worker_probe.cpu.load(Ordering::Relaxed),
                        khal::percpu::this_cpu_id().as_usize()
                    );
                    core::assert!(worker_probe.loads.load(Ordering::Relaxed) > 0);
                }
                crate::wait_for_completion(&worker_release).unwrap();
                crate::yield_now();
                // Exit does not return or drop the task's stack locals. The
                // observer must still receive the terminal put.
                drop(worker_probe);
                drop(worker_release);
                crate::exit(7);
            }).unwrap(),
            "observer-lifecycle".into(),
            0x4000,
        )
        .with_schedule_observer(probe.clone()),
    );
    while !probe.has_blocked.load(Ordering::Acquire) {
        crate::yield_now();
    }
    release.complete_all();
    let status = task.join();
    // join observes the exit code, not completion of the outgoing stack switch.
    while !probe.has_exited.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert_eq!(status, 7);
    assert!(!probe.is_invalid.load(Ordering::Relaxed));
    assert_eq!(probe.cpu.load(Ordering::Relaxed), NO_CPU);
    assert_eq!(
        probe.loads.load(Ordering::Relaxed),
        probe.puts.load(Ordering::Relaxed)
    );
    assert!(probe.loads.load(Ordering::Relaxed) >= 2);
}

#[cfg(feature = "smp")]
#[def_test]
fn migration_puts_source_before_loading_destination() {
    use kcpu_id_map::KCpuMaskExt;

    if crate::active_cpu_num() < 2 {
        return unittest::TestResult::Ignored;
    }
    let probe = Arc::new(Probe::new());
    let worker_probe = probe.clone();
    let task = crate::spawn_task(
        crate::TaskInner::new_pidless_kthread_with_entry(
            crate::PreparedTaskEntry::new(move || {
                for cpu in [0, 1, 0, 1] {
                    core::assert!(crate::set_current_affinity(
                        crate::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu))
                    ));
                    let _irq = kspin::IrqSave::new();
                    core::assert_eq!(khal::percpu::this_cpu_id(), LogicalCpuId::new(cpu));
                    core::assert_eq!(worker_probe.cpu.load(Ordering::Relaxed), cpu);
                }
            }).unwrap(),
            "observer-migration".into(),
            0x4000,
        )
        .with_schedule_observer(probe.clone()),
    );
    let status = task.join();
    while !probe.has_exited.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert_eq!(status, 0);
    assert!(!probe.is_invalid.load(Ordering::Relaxed));
    assert_eq!(probe.visited.load(Ordering::Relaxed) & 3, 3);
    assert_eq!(probe.cpu.load(Ordering::Relaxed), NO_CPU);
    assert_eq!(
        probe.loads.load(Ordering::Relaxed),
        probe.puts.load(Ordering::Relaxed)
    );
}

#[def_test]
fn same_task_reselection_does_not_put_or_reload() {
    let probe = Arc::new(Probe::new());
    let worker_probe = probe.clone();
    let task = crate::spawn_task(
        crate::TaskInner::new_pidless_kthread_with_entry(
            crate::PreparedTaskEntry::new(move || {
                let _guard = kspin::NoPreemptIrqSave::new();
                let before = (
                    worker_probe.loads.load(Ordering::Relaxed),
                    worker_probe.puts.load(Ordering::Relaxed),
                );
                let current = crate::current();
                let next = current.clone();
                // Exercise the real identity fast path without relying on
                // there being no other runnable tasks in the harness.
                super::super::current_run_queue_ref().switch_to(
                    current,
                    next,
                    crate::TaskState::Running,
                );
                core::assert_eq!(
                    before,
                    (
                        worker_probe.loads.load(Ordering::Relaxed),
                        worker_probe.puts.load(Ordering::Relaxed),
                    )
                );
            }).unwrap(),
            "observer-same-task".into(),
            0x4000,
        )
        .with_schedule_observer(probe.clone()),
    );
    let status = task.join();
    while !probe.has_exited.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert_eq!(status, 0);
    assert!(!probe.is_invalid.load(Ordering::Relaxed));
}

struct AlarmObserver {
    probe: Probe,
    alarm: crate::Timer<Alarm>,
}

struct Alarm(Arc<Completion>);

impl crate::TimerHandler for Alarm {
    fn on_expiration(&self, _: crate::TimerControl<'_>, _: crate::Expiration) {
        self.0.complete_all();
    }
}

// SAFETY: preparation happens before task publication. Callbacks use bounded
// atomic checks, nonwaiting cancellation and allocation-free timer start;
// expiry wakes only through Completion, outside scheduler notification.
unsafe impl TaskScheduleObserver for AlarmObserver {
    unsafe fn schedule_in(&self, cpu: LogicalCpuId) {
        // SAFETY: the scheduler supplied the same current-task/IRQ exclusion
        // required by the nested probe, without modifying that context.
        unsafe { self.probe.schedule_in(cpu) };
        let _ = self.alarm.try_cancel();
    }

    unsafe fn schedule_out(&self, cpu: LogicalCpuId, reason: ScheduleOutReason) {
        // SAFETY: forwarding the scheduler's current-task transition unchanged.
        unsafe { self.probe.schedule_out(cpu, reason) };
        if reason == ScheduleOutReason::Blocked {
            let deadline = khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(2);
            // SAFETY: the harness runs after scheduler/timer initialization;
            // Alarm only completes a retained Completion in timer IRQ context.
            core::assert!(unsafe { self.alarm.start(deadline) }.is_ok());
        }
    }
}

#[def_test]
fn prepared_alarm_can_wake_a_task_put_by_the_scheduler() {
    let done = Arc::new(Completion::new());
    let observer = Arc::new(AlarmObserver {
        probe: Probe::new(),
        alarm: crate::Timer::new(Alarm(done.clone())),
    });
    let task = crate::spawn_task(
        crate::TaskInner::new_pidless_kthread_with_entry(
            crate::PreparedTaskEntry::new(move || crate::wait_for_completion(&done).unwrap()).unwrap(),
            "observer-alarm".into(),
            0x4000,
        )
        .with_schedule_observer(observer.clone()),
    );
    let status = task.join();
    while !observer.probe.has_exited.load(Ordering::Acquire) {
        crate::yield_now();
    }
    // Draining belongs outside the scheduler notification, even though the
    // timer's completion has already let the task return.
    observer.alarm.cancel_sync();
    assert_eq!(status, 0);
    assert!(observer.probe.has_blocked.load(Ordering::Acquire));
    assert!(!observer.probe.is_invalid.load(Ordering::Relaxed));
    assert_eq!(
        observer.probe.loads.load(Ordering::Relaxed),
        observer.probe.puts.load(Ordering::Relaxed)
    );
}

struct ExclusiveCpuProbe {
    probe: Probe,
    owner: Arc<AtomicUsize>,
    key: usize,
}

// SAFETY: callbacks only forward to the atomic probe and update a shared
// atomic owner. Both observed tasks are bound to the same CPU before activation.
unsafe impl TaskScheduleObserver for ExclusiveCpuProbe {
    unsafe fn schedule_in(&self, cpu: LogicalCpuId) {
        // SAFETY: forwards the scheduler-supplied load context unchanged.
        unsafe { self.probe.schedule_in(cpu) };
        if self.owner.swap(self.key, Ordering::Relaxed) != 0 {
            self.probe.is_invalid.store(true, Ordering::Relaxed);
        }
    }

    unsafe fn schedule_out(&self, cpu: LogicalCpuId, reason: ScheduleOutReason) {
        if self.owner.swap(0, Ordering::Relaxed) != self.key {
            self.probe.is_invalid.store(true, Ordering::Relaxed);
        }
        // SAFETY: forwards the scheduler-supplied put context unchanged.
        unsafe { self.probe.schedule_out(cpu, reason) };
    }
}

#[def_test]
fn cpu_reuse_releases_the_previous_observers_ownership() {
    use kcpu_id_map::KCpuMaskExt;

    let owner = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Completion::new());
    let release = Arc::new(Completion::new());
    let first = Arc::new(ExclusiveCpuProbe {
        probe: Probe::new(),
        owner: owner.clone(),
        key: 1,
    });
    let second = Arc::new(ExclusiveCpuProbe {
        probe: Probe::new(),
        owner: owner.clone(),
        key: 2,
    });
    let worker_entered = entered.clone();
    let worker_release = release.clone();
    let first_task = crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new(move || {
            worker_entered.complete_all();
            crate::wait_for_completion(&worker_release).unwrap();
        }).unwrap(),
        "observer-owner-a".into(),
        0x4000,
    )
    .with_schedule_observer(first.clone());
    first_task.set_cpumask(crate::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    let second_task = crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new(move || {
            crate::wait_for_completion(&entered).unwrap();
            release.complete_all();
        }).unwrap(),
        "observer-owner-b".into(),
        0x4000,
    )
    .with_schedule_observer(second.clone());
    second_task.set_cpumask(crate::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    let first_task = crate::spawn_task(first_task);
    let second_task = crate::spawn_task(second_task);
    let statuses = [first_task.join(), second_task.join()];
    while !first.probe.has_exited.load(Ordering::Acquire)
        || !second.probe.has_exited.load(Ordering::Acquire)
    {
        crate::yield_now();
    }
    assert_eq!(statuses, [0, 0]);
    assert!(!first.probe.is_invalid.load(Ordering::Relaxed));
    assert!(!second.probe.is_invalid.load(Ordering::Relaxed));
    assert_eq!(owner.load(Ordering::Relaxed), 0);
}
