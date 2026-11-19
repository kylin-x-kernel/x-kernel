// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fault injection into the production PM executor with test-owned resources.
//! Never freeze the live kernel or inject offsets into its global timekeeper.

use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::RefCell;

use unittest::{assert, assert_eq, def_test};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    None,
    Prepare,
    Tasks,
    Begin,
    Pause,
    Freeze,
    Mark,
    Enter,
    Settle,
    EnterAndSettle,
}

struct Probe {
    log: Vec<&'static str>,
    fault: Fault,
    irqs: bool,
    prepared: bool,
    tasks_frozen: bool,
    accounting: bool,
    paused: bool,
    restored: bool,
    frozen: bool,
    hardware_ready: bool,
    injected: u64,
    queue_count: usize,
    frozen_queues: usize,
    queue_failure: Option<WorkqueueFreezeError>,
}
impl Probe {
    fn new(fault: Fault) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            log: Vec::new(),
            fault,
            irqs: true,
            prepared: false,
            tasks_frozen: false,
            accounting: false,
            paused: false,
            restored: false,
            frozen: false,
            hardware_ready: true,
            injected: 0,
            queue_count: 0,
            frozen_queues: 0,
            queue_failure: None,
        }))
    }
}
struct Ops(Rc<RefCell<Probe>>);
struct Source(Rc<RefCell<Probe>>);
struct Session(Rc<RefCell<Probe>>);
struct Critical(Rc<RefCell<Probe>>);
struct TaskGuard(Rc<RefCell<Probe>>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.prepared && p.irqs && !p.accounting && !p.paused && !p.frozen);
        core::assert!(p.tasks_frozen);
        core::assert_eq!(p.frozen_queues, 0);
        p.tasks_frozen = false;
        p.log.push("tasks-thaw");
    }
}
impl Drop for Critical {
    fn drop(&mut self) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.frozen && p.hardware_ready);
        p.irqs = true;
        p.log.push("irq-on");
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut p = self.0.borrow_mut();
        if p.accounting {
            core::assert!(!p.frozen && p.hardware_ready);
            p.accounting = false;
            p.log.push("cancel");
        }
    }
}
impl SleepSource for Source {
    type Session<'a> = Session;

    fn begin(&mut self) -> Result<Self::Session<'_>, RtcSleepError> {
        let mut p = self.0.borrow_mut();
        core::assert!(p.irqs && p.prepared && !p.frozen);
        p.log.push("rtc-begin");
        if p.fault == Fault::Begin {
            return Err(RtcSleepError::ReadFailed);
        }
        p.accounting = true;
        Ok(Session(self.0.clone()))
    }
}
impl SleepSession for Session {
    fn mark_frozen(&mut self) -> Result<(), RtcSleepError> {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && p.frozen && p.paused && p.accounting);
        p.log.push("mark");
        if p.fault == Fault::Mark {
            return Err(RtcSleepError::AlreadySuspended);
        }
        Ok(())
    }

    fn settle(&mut self) -> Result<TimeSpan, RtcSleepError> {
        let mut p = self.0.borrow_mut();
        core::assert!(p.irqs && !p.frozen && p.restored && p.paused && p.accounting);
        p.log.push("settle");
        if matches!(p.fault, Fault::Settle | Fault::EnterAndSettle) {
            return Err(RtcSleepError::ReadFailed);
        }
        p.accounting = false;
        p.injected += 7;
        Ok(TimeSpan::from_secs(7))
    }
}
impl SuspendOps for Ops {
    type Clock = ();
    type Critical = Critical;
    type Events = ();
    type Tasks = TaskGuard;

    fn freeze_workqueues(&mut self) -> Result<(), WorkqueueFreezeError> {
        let mut p = self.0.borrow_mut();
        core::assert!(p.tasks_frozen && p.prepared && p.irqs && !p.accounting);
        for index in 0..p.queue_count {
            p.log
                .push(["queue0-freeze", "queue1-freeze", "queue2-freeze"][index]);
            if let Some(error) = p.queue_failure.filter(|error| error.index == index) {
                return Err(error);
            }
            p.frozen_queues += 1;
        }
        Ok(())
    }

    fn thaw_workqueues(&mut self) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.prepared && p.irqs && !p.frozen && !p.paused && !p.accounting);
        while p.frozen_queues != 0 {
            p.frozen_queues -= 1;
            let index = p.frozen_queues;
            p.log
                .push(["queue0-thaw", "queue1-thaw", "queue2-thaw"][index]);
        }
    }

    fn freeze_tasks(&mut self) -> Result<TaskGuard, FreezeError> {
        let mut p = self.0.borrow_mut();
        core::assert!(p.prepared && p.irqs && !p.accounting);
        p.log.push("tasks-freeze");
        if p.fault == Fault::Tasks {
            return Err(FreezeError::TimedOut);
        }
        p.tasks_frozen = true;
        Ok(TaskGuard(self.0.clone()))
    }

    fn prepare(&mut self) -> KResult<()> {
        let mut p = self.0.borrow_mut();
        core::assert!(p.irqs && !p.prepared);
        p.log.push("prepare");
        if p.fault == Fault::Prepare {
            return Err(KError::OperationNotSupported);
        }
        p.prepared = true;
        Ok(())
    }

    fn finish(&mut self) {
        let mut p = self.0.borrow_mut();
        core::assert!(p.prepared && p.irqs && !p.frozen && !p.paused && !p.accounting);
        p.prepared = false;
        p.log.push("finish");
    }

    fn mask_irqs(&mut self) -> Self::Critical {
        let mut p = self.0.borrow_mut();
        core::assert!(p.irqs);
        p.irqs = false;
        p.log.push("irq-off");
        Critical(self.0.clone())
    }

    fn pause_events(&mut self) -> Result<(), TimerSuspendError> {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && !p.paused);
        p.log.push("pause");
        if p.fault == Fault::Pause {
            return Err(TimerSuspendError::InvalidContext);
        }
        p.paused = true;
        Ok(())
    }

    fn freeze(&mut self) -> Result<(), MonotonicSuspendError> {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && p.paused);
        p.log.push("freeze");
        if p.fault == Fault::Freeze {
            return Err(MonotonicSuspendError::TransitionInProgress);
        }
        p.frozen = true;
        Ok(())
    }

    fn enter(&mut self) -> KResult<()> {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && p.frozen && p.paused);
        p.hardware_ready = false;
        p.log.push("enter");
        if matches!(p.fault, Fault::Enter | Fault::EnterAndSettle) {
            return Err(KError::OperationNotSupported);
        }
        Ok(())
    }

    fn restore_platform(&mut self) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && p.frozen && p.paused);
        p.hardware_ready = true;
        p.log.push("platform-restore");
    }

    fn resume_clock(&mut self, _: &()) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && p.frozen && p.hardware_ready);
        p.frozen = false;
        p.log.push("clock-resume");
    }

    fn restore_events(&mut self, _: &mut ()) {
        let mut p = self.0.borrow_mut();
        core::assert!(!p.irqs && !p.frozen && p.paused && p.hardware_ready);
        p.restored = true;
        p.log.push("event-restore");
    }

    fn resume_events(&mut self, _: &mut ()) {
        let mut p = self.0.borrow_mut();
        core::assert!(p.irqs && !p.frozen && p.paused && p.restored && !p.accounting);
        p.paused = false;
        p.restored = false;
        p.log.push("event-resume");
    }
}

fn attempt(fault: Fault) -> (Result<TimeSpan, SuspendError>, Rc<RefCell<Probe>>) {
    let probe = Probe::new(fault);
    let result = run(
        &mut Ops(probe.clone()),
        &mut Source(probe.clone()),
        &mut DeviceSet::new(&mut []),
    );
    let p = probe.borrow();
    core::assert!(p.irqs && !p.frozen && !p.paused && !p.accounting && !p.prepared);
    core::assert_eq!(
        p.log.iter().filter(|&&s| s == "finish").count(),
        usize::from(fault != Fault::Prepare)
    );
    drop(p);
    (result, probe)
}

#[def_test]
fn admission_rejects_multiple_cpus_and_non_sleepable_context() {
    let topology_reads = core::cell::Cell::new(0);
    assert_eq!(
        validate_context(false, || {
            topology_reads.set(topology_reads.get() + 1);
            4
        }),
        Err(SuspendError::InvalidContext)
    );
    assert_eq!(topology_reads.get(), 0);
    assert_eq!(
        validate_context(false, || 1),
        Err(SuspendError::InvalidContext)
    );
    assert_eq!(
        validate_context(true, || 4),
        Err(SuspendError::MultipleCpus)
    );
    assert_eq!(
        validate_context(true, || 0),
        Err(SuspendError::MultipleCpus)
    );
    assert_eq!(validate_context(true, || 1), Ok(()));
}

#[def_test]
fn transaction_claim_rejects_nested_entry_and_releases_for_retry() {
    let active = AtomicBool::new(false);
    let first = Admission::acquire(&active).unwrap();
    assert!(matches!(
        Admission::acquire(&active),
        Err(SuspendError::Busy)
    ));
    drop(first);
    let retry = Admission::acquire(&active).unwrap();
    drop(retry);
    assert!(!active.load(Ordering::Relaxed));
}

#[def_test]
fn successful_suspend_restores_time_before_sleep_accounting_and_events() {
    let (result, probe) = attempt(Fault::None);
    assert_eq!(result, Ok(TimeSpan::from_secs(7)));
    let p = probe.borrow();
    assert_eq!(p.injected, 7);
    assert_eq!(
        p.log,
        vec![
            "prepare",
            "tasks-freeze",
            "rtc-begin",
            "irq-off",
            "pause",
            "freeze",
            "mark",
            "enter",
            "platform-restore",
            "clock-resume",
            "event-restore",
            "irq-on",
            "settle",
            "event-resume",
            "finish",
            "tasks-thaw"
        ]
    );
}

#[def_test]
fn unsupported_preparation_never_touches_clocks_or_events() {
    let (result, probe) = attempt(Fault::Prepare);
    assert_eq!(
        result,
        Err(SuspendError::Prepare(KError::OperationNotSupported))
    );
    assert_eq!(probe.borrow().log, vec!["prepare"]);
}

#[def_test]
fn failed_initial_rtc_sample_releases_only_platform_preparation() {
    let (result, probe) = attempt(Fault::Begin);
    assert_eq!(
        result,
        Err(SuspendError::RtcPrepare(RtcSleepError::ReadFailed))
    );
    assert_eq!(
        probe.borrow().log,
        vec![
            "prepare",
            "tasks-freeze",
            "rtc-begin",
            "finish",
            "tasks-thaw"
        ]
    );
}

#[def_test]
fn failed_event_pause_cancels_accounting_without_freezing_time() {
    let (result, probe) = attempt(Fault::Pause);
    assert_eq!(
        result,
        Err(SuspendError::TimerPause(TimerSuspendError::InvalidContext))
    );
    assert_eq!(
        probe.borrow().log,
        vec![
            "prepare",
            "tasks-freeze",
            "rtc-begin",
            "irq-off",
            "pause",
            "irq-on",
            "cancel",
            "finish",
            "tasks-thaw"
        ]
    );
}

#[def_test]
fn failed_clock_freeze_restores_events_and_cancels_before_unquiesce() {
    let (result, probe) = attempt(Fault::Freeze);
    assert_eq!(
        result,
        Err(SuspendError::ClockFreeze(
            MonotonicSuspendError::TransitionInProgress
        ))
    );
    assert_eq!(
        probe.borrow().log,
        vec![
            "prepare",
            "tasks-freeze",
            "rtc-begin",
            "irq-off",
            "pause",
            "freeze",
            "event-restore",
            "irq-on",
            "cancel",
            "event-resume",
            "finish",
            "tasks-thaw"
        ]
    );
}

#[def_test]
fn failed_freeze_marker_unfreezes_owned_clock_before_cancelling() {
    let (result, probe) = attempt(Fault::Mark);
    assert_eq!(
        result,
        Err(SuspendError::MarkFrozen(RtcSleepError::AlreadySuspended))
    );
    let p = probe.borrow();
    assert!(!p.log.contains(&"enter"));
    assert!(!p.log.contains(&"settle"));
    assert!(
        p.log.iter().position(|&s| s == "clock-resume") < p.log.iter().position(|&s| s == "cancel")
    );
}

#[def_test]
fn failed_platform_entry_still_restores_and_settles_excluded_interval() {
    let (result, probe) = attempt(Fault::Enter);
    assert_eq!(
        result,
        Err(SuspendError::Resume {
            entry_error: Some(KError::OperationNotSupported),
            sleep_time: Ok(TimeSpan::from_secs(7))
        })
    );
    let p = probe.borrow();
    assert_eq!(p.injected, 7);
    assert!(p.log.contains(&"platform-restore"));
    assert!(!p.log.contains(&"cancel"));
}

#[def_test]
fn failed_resume_sample_cancels_without_hiding_missing_sleep_as_zero() {
    let (result, probe) = attempt(Fault::Settle);
    assert_eq!(
        result,
        Err(SuspendError::Resume {
            entry_error: None,
            sleep_time: Err(RtcSleepError::ReadFailed)
        })
    );
    let p = probe.borrow();
    assert_eq!(p.injected, 0);
    assert!(
        p.log.iter().position(|&s| s == "cancel") < p.log.iter().position(|&s| s == "event-resume")
    );
}

#[def_test]
fn entry_and_compensation_errors_are_both_retained_after_cleanup() {
    let (result, probe) = attempt(Fault::EnterAndSettle);
    assert_eq!(
        result,
        Err(SuspendError::Resume {
            entry_error: Some(KError::OperationNotSupported),
            sleep_time: Err(RtcSleepError::ReadFailed)
        })
    );
    assert_eq!(probe.borrow().injected, 0);
}

struct RefusingPlatform(core::cell::Cell<usize>);
// SAFETY: prepare always refuses without touching hardware or quiescing tasks,
// so no successful preparation or frozen interval can reach the other hooks.
// Those hooks are non-blocking and make no hardware/state changes either.
unsafe impl PlatformSuspendOps for RefusingPlatform {
    fn prepare(&mut self) -> KResult<()> {
        self.0.set(self.0.get() + 1);
        Err(KError::OperationNotSupported)
    }

    fn enter(&mut self) -> KResult<()> {
        Err(KError::OperationNotSupported)
    }

    fn restore(&mut self) {}

    fn finish(&mut self) {}
}
struct UnreadClock(Rc<core::cell::Cell<usize>>);
impl PersistentClock for UnreadClock {
    fn read(&mut self) -> Option<ktime_types::SystemTime> {
        self.0.set(self.0.get() + 1);
        None
    }

    fn resolution(&self) -> TimeSpan {
        TimeSpan::from_secs(1)
    }
}

#[def_test]
fn public_admission_or_refused_preparation_never_reads_source() {
    assert!(ktask::is_sleepable_context());
    let reads = Rc::new(core::cell::Cell::new(0));
    let mut source = RtcSleepAccounting::new(
        UnreadClock(reads.clone()),
        TimeSpan::from_secs(60),
        TimeSpan::from_millis(10),
    )
    .unwrap();
    let mut platform = RefusingPlatform(core::cell::Cell::new(0));
    let result = suspend_uniprocessor(
        &mut platform,
        &mut source,
        SuspendTasks {
            exempt: &[],
            timeout: TimeSpan::from_secs(1),
            cancellation: &FreezeCancellation::new(),
        },
    );
    if kcpu_id_map::nr_cpus() == 1 {
        assert_eq!(
            result,
            Err(SuspendError::Prepare(KError::OperationNotSupported))
        );
        assert_eq!(platform.0.get(), 1);
    } else {
        assert_eq!(result, Err(SuspendError::MultipleCpus));
        assert_eq!(platform.0.get(), 0);
    }
    assert_eq!(reads.get(), 0);
}

#[def_test]
fn failed_task_acknowledgment_finishes_without_sampling_or_freezing_time() {
    let (result, probe) = attempt(Fault::Tasks);
    assert_eq!(result, Err(SuspendError::TaskFreeze(FreezeError::TimedOut)));
    assert_eq!(
        probe.borrow().log,
        vec!["prepare", "tasks-freeze", "finish"]
    );
}

struct DeviceProbe {
    index: usize,
    probe: Rc<RefCell<Probe>>,
    fail_at: Rc<core::cell::Cell<Option<usize>>>,
    is_suspended: bool,
}

// SAFETY: these fixtures only mutate test-owned state. They never quiesce live
// hardware, wait for frozen tasks or change real IRQ/time state. Failed suspend
// leaves the fixture active; resume is infallible and balances one success.
unsafe impl DeviceSuspendOps for DeviceProbe {
    fn suspend(&mut self) -> KResult<()> {
        let mut p = self.probe.borrow_mut();
        core::assert!(p.tasks_frozen && p.prepared && p.irqs);
        core::assert_eq!(p.frozen_queues, p.queue_count);
        core::assert!(!p.frozen && !p.paused && !p.accounting && !self.is_suspended);
        p.log
            .push(["device0-suspend", "device1-suspend", "device2-suspend"][self.index]);
        if self.fail_at.get() == Some(self.index) {
            return Err(KError::OperationNotSupported);
        }
        self.is_suspended = true;
        Ok(())
    }

    fn resume(&mut self) {
        let mut p = self.probe.borrow_mut();
        core::assert!(p.tasks_frozen && p.prepared && p.irqs && p.hardware_ready);
        core::assert_eq!(p.frozen_queues, p.queue_count);
        core::assert!(!p.frozen && !p.paused && !p.accounting && self.is_suspended);
        p.log
            .push(["device0-resume", "device1-resume", "device2-resume"][self.index]);
        self.is_suspended = false;
    }
}

fn devices_attempt(
    fault: Fault,
    fail_at: Option<usize>,
) -> (Result<TimeSpan, SuspendError>, Rc<RefCell<Probe>>) {
    let probe = Probe::new(fault);
    let failure = Rc::new(core::cell::Cell::new(fail_at));
    let mut fixtures: [_; 3] = core::array::from_fn(|index| DeviceProbe {
        index,
        probe: probe.clone(),
        fail_at: failure.clone(),
        is_suspended: false,
    });
    let mut devices: [&mut dyn DeviceSuspendOps; 3] = fixtures.each_mut().map(|d| d as _);
    let result = run(
        &mut Ops(probe.clone()),
        &mut Source(probe.clone()),
        &mut DeviceSet::new(&mut devices),
    );
    core::assert!(fixtures.iter().all(|device| !device.is_suspended));
    let p = probe.borrow();
    core::assert!(p.irqs && !p.prepared && !p.tasks_frozen);
    core::assert!(!p.frozen && !p.paused && !p.accounting);
    drop(p);
    (result, probe)
}

#[def_test]
fn device_suspend_and_resume_bracket_the_time_transaction_and_preserve_dependencies() {
    let (result, probe) = devices_attempt(Fault::None, None);
    assert_eq!(result, Ok(TimeSpan::from_secs(7)));
    assert_eq!(
        probe.borrow().log,
        vec![
            "prepare",
            "tasks-freeze",
            "device0-suspend",
            "device1-suspend",
            "device2-suspend",
            "rtc-begin",
            "irq-off",
            "pause",
            "freeze",
            "mark",
            "enter",
            "platform-restore",
            "clock-resume",
            "event-restore",
            "irq-on",
            "settle",
            "event-resume",
            "device2-resume",
            "device1-resume",
            "device0-resume",
            "finish",
            "tasks-thaw",
        ]
    );
}

#[def_test]
fn device_failure_identifies_the_device_and_rolls_back_only_successful_predecessors() {
    for index in 0..3 {
        let (result, probe) = devices_attempt(Fault::None, Some(index));
        assert_eq!(
            result,
            Err(SuspendError::DeviceSuspend(DeviceSuspendError {
                index,
                error: KError::OperationNotSupported,
            }))
        );
        let mut expected = vec!["prepare", "tasks-freeze"];
        expected.extend_from_slice(
            &["device0-suspend", "device1-suspend", "device2-suspend"][..=index],
        );
        expected.extend(
            ["device0-resume", "device1-resume", "device2-resume"][..index]
                .iter()
                .rev()
                .copied(),
        );
        expected.extend_from_slice(&["finish", "tasks-thaw"]);
        assert_eq!(probe.borrow().log, expected);
    }
}

#[def_test]
fn every_later_failure_restores_devices_before_platform_release_and_task_thaw() {
    for fault in [
        Fault::Begin,
        Fault::Pause,
        Fault::Freeze,
        Fault::Mark,
        Fault::Enter,
        Fault::Settle,
        Fault::EnterAndSettle,
    ] {
        let (result, probe) = devices_attempt(fault, None);
        assert!(result.is_err());
        let p = probe.borrow();
        assert_eq!(
            &p.log[..5],
            &[
                "prepare",
                "tasks-freeze",
                "device0-suspend",
                "device1-suspend",
                "device2-suspend"
            ]
        );
        assert_eq!(
            &p.log[p.log.len() - 5..],
            &[
                "device2-resume",
                "device1-resume",
                "device0-resume",
                "finish",
                "tasks-thaw"
            ]
        );
        for label in ["device0-resume", "device1-resume", "device2-resume"] {
            assert_eq!(p.log.iter().filter(|&&entry| entry == label).count(), 1);
        }
    }
}

#[def_test]
fn platform_or_freezer_rejection_never_invokes_device_callbacks() {
    for fault in [Fault::Prepare, Fault::Tasks] {
        let (result, probe) = devices_attempt(fault, None);
        assert!(result.is_err());
        assert!(
            !probe
                .borrow()
                .log
                .iter()
                .any(|entry| entry.starts_with("device"))
        );
    }
}

#[def_test]
fn device_rollback_allows_retry_without_double_resume() {
    let probe = Probe::new(Fault::None);
    let failure = Rc::new(core::cell::Cell::new(Some(1)));
    let mut fixtures: [_; 3] = core::array::from_fn(|index| DeviceProbe {
        index,
        probe: probe.clone(),
        fail_at: failure.clone(),
        is_suspended: false,
    });
    let mut devices: [&mut dyn DeviceSuspendOps; 3] = fixtures.each_mut().map(|d| d as _);
    let mut set = DeviceSet::new(&mut devices);
    assert!(
        run(
            &mut Ops(probe.clone()),
            &mut Source(probe.clone()),
            &mut set
        )
        .is_err()
    );
    failure.set(None);
    probe.borrow_mut().log.clear();
    assert_eq!(
        run(
            &mut Ops(probe.clone()),
            &mut Source(probe.clone()),
            &mut set
        ),
        Ok(TimeSpan::from_secs(7))
    );
    let log = probe.borrow().log.clone();
    set.resume();
    assert_eq!(probe.borrow().log, log);
}

#[def_test]
fn public_device_entry_rejects_before_device_or_rtc_access() {
    let reads = Rc::new(core::cell::Cell::new(0));
    let mut source = RtcSleepAccounting::new(
        UnreadClock(reads.clone()),
        TimeSpan::from_secs(60),
        TimeSpan::from_millis(10),
    )
    .unwrap();
    let probe = Probe::new(Fault::None);
    let mut device = DeviceProbe {
        index: 0,
        probe: probe.clone(),
        fail_at: Rc::new(core::cell::Cell::new(None)),
        is_suspended: false,
    };
    let mut platform = RefusingPlatform(core::cell::Cell::new(0));
    let result = suspend_uniprocessor_with_devices(
        &mut platform,
        &mut source,
        SuspendTasks {
            exempt: &[],
            timeout: TimeSpan::from_secs(1),
            cancellation: &FreezeCancellation::new(),
        },
        &mut [&mut device],
    );
    if kcpu_id_map::nr_cpus() == 1 {
        assert_eq!(
            result,
            Err(SuspendError::Prepare(KError::OperationNotSupported))
        );
        assert_eq!(platform.0.get(), 1);
    } else {
        assert_eq!(result, Err(SuspendError::MultipleCpus));
        assert_eq!(platform.0.get(), 0);
    }
    assert!(probe.borrow().log.is_empty());
    assert_eq!(reads.get(), 0);
}

fn queues_attempt(
    fault: Fault,
    queue_failure: Option<WorkqueueFreezeError>,
    device_failure: Option<usize>,
) -> (Result<TimeSpan, SuspendError>, Rc<RefCell<Probe>>) {
    let probe = Probe::new(fault);
    probe.borrow_mut().queue_count = 3;
    probe.borrow_mut().queue_failure = queue_failure;
    let mut device = DeviceProbe {
        index: 0,
        probe: probe.clone(),
        fail_at: Rc::new(core::cell::Cell::new(device_failure)),
        is_suspended: false,
    };
    let result = run(
        &mut Ops(probe.clone()),
        &mut Source(probe.clone()),
        &mut DeviceSet::new(&mut [&mut device]),
    );
    let p = probe.borrow();
    core::assert!(!p.prepared && !p.tasks_frozen && p.frozen_queues == 0);
    core::assert!(p.irqs && !p.frozen && !p.paused && !p.accounting);
    core::assert!(!device.is_suspended);
    drop(p);
    (result, probe)
}

#[def_test]
fn queues_pause_after_tasks_and_thaw_after_devices_and_platform_before_tasks() {
    let (result, probe) = queues_attempt(Fault::None, None, None);
    assert_eq!(result, Ok(TimeSpan::from_secs(7)));
    let p = probe.borrow();
    assert_eq!(
        &p.log[..6],
        &[
            "prepare",
            "tasks-freeze",
            "queue0-freeze",
            "queue1-freeze",
            "queue2-freeze",
            "device0-suspend",
        ]
    );
    assert_eq!(
        &p.log[p.log.len() - 6..],
        &[
            "device0-resume",
            "finish",
            "queue2-thaw",
            "queue1-thaw",
            "queue0-thaw",
            "tasks-thaw",
        ]
    );
}

#[def_test]
fn queue_failure_unwinds_only_successful_prefix_without_device_or_time_access() {
    for error in [
        kworkng::QueueFreezeError::TimedOut,
        kworkng::QueueFreezeError::Cancelled,
        kworkng::QueueFreezeError::Busy,
        kworkng::QueueFreezeError::Closed,
    ] {
        for index in 0..3 {
            let failure = WorkqueueFreezeError { index, error };
            let (result, probe) = queues_attempt(Fault::None, Some(failure), None);
            assert_eq!(result, Err(SuspendError::WorkqueueFreeze(failure)));
            let mut expected = vec!["prepare", "tasks-freeze"];
            expected
                .extend_from_slice(&["queue0-freeze", "queue1-freeze", "queue2-freeze"][..=index]);
            expected.push("finish");
            expected.extend(
                ["queue0-thaw", "queue1-thaw", "queue2-thaw"][..index]
                    .iter()
                    .rev()
                    .copied(),
            );
            expected.push("tasks-thaw");
            assert_eq!(probe.borrow().log, expected);
        }
    }
}

#[def_test]
fn queue_guards_cover_every_device_time_and_entry_failure() {
    for fault in [
        Fault::None,
        Fault::Begin,
        Fault::Pause,
        Fault::Freeze,
        Fault::Mark,
        Fault::Enter,
        Fault::Settle,
        Fault::EnterAndSettle,
    ] {
        for device_failure in [None, Some(0)] {
            let (_, probe) = queues_attempt(fault, None, device_failure);
            let p = probe.borrow();
            assert_eq!(
                &p.log[p.log.len() - 5..],
                &[
                    "finish",
                    "queue2-thaw",
                    "queue1-thaw",
                    "queue0-thaw",
                    "tasks-thaw",
                ]
            );
            for label in ["queue0-thaw", "queue1-thaw", "queue2-thaw"] {
                assert_eq!(p.log.iter().filter(|&&s| s == label).count(), 1);
            }
        }
    }
}

#[def_test]
fn platform_and_task_rejection_never_attempt_queue_freeze() {
    for fault in [Fault::Prepare, Fault::Tasks] {
        let (_, probe) = queues_attempt(fault, None, None);
        assert!(!probe.borrow().log.iter().any(|s| s.starts_with("queue")));
    }
}

#[def_test]
fn queue_rollback_and_success_can_be_repeated_without_stale_ownership() {
    let probe = Probe::new(Fault::None);
    probe.borrow_mut().queue_count = 3;
    let mut ops = Ops(probe.clone());
    for failure in [
        Some(WorkqueueFreezeError {
            index: 1,
            error: kworkng::QueueFreezeError::Cancelled,
        }),
        None,
        None,
    ] {
        probe.borrow_mut().queue_failure = failure;
        let result = run(
            &mut ops,
            &mut Source(probe.clone()),
            &mut DeviceSet::new(&mut []),
        );
        assert_eq!(result.is_err(), failure.is_some());
        assert_eq!(probe.borrow().frozen_queues, 0);
        assert!(!probe.borrow().tasks_frozen);
    }
}

#[def_test(serial)]
fn public_workqueue_entry_rejects_without_pausing_queues_or_reading_rtc() {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let queue = kworkng::NormalQueue::new(
        "pm-public-admission",
        kworkng::QueueConfig::new(&cpus, kworkng::CpuPolicy::CurrentCpu, 1).unwrap(),
    )
    .unwrap();
    let reads = Rc::new(core::cell::Cell::new(0));
    let mut source = RtcSleepAccounting::new(
        UnreadClock(reads.clone()),
        TimeSpan::from_secs(60),
        TimeSpan::from_millis(10),
    )
    .unwrap();
    let mut platform = RefusingPlatform(core::cell::Cell::new(0));
    let task_cancellation = FreezeCancellation::new();
    let queue_cancellation = kworkng::QueueFreezeCancellation::new();
    let result = suspend_uniprocessor_with_workqueues(
        &mut platform,
        &mut source,
        SuspendTasks {
            exempt: &[],
            timeout: TimeSpan::ZERO,
            cancellation: &task_cancellation,
        },
        SuspendWorkqueues {
            queues: core::slice::from_ref(&queue),
            timeout: TimeSpan::ZERO,
            cancellation: &queue_cancellation,
        },
        &mut [],
    );
    if kcpu_id_map::nr_cpus() == 1 {
        assert_eq!(
            result,
            Err(SuspendError::Prepare(KError::OperationNotSupported))
        );
        assert_eq!(platform.0.get(), 1);
    } else {
        assert_eq!(result, Err(SuspendError::MultipleCpus));
        assert_eq!(platform.0.get(), 0);
    }
    assert_eq!(reads.get(), 0);
    drop(queue.freeze(TimeSpan::ZERO, &queue_cancellation).unwrap());
    queue.close_and_drain().unwrap();
}
