// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private startup ownership and progress policy fixtures.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::AtomicUsize;

use krcu_core::CallbackNode;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[repr(C)]
struct BudgetProbe {
    node: CallbackNode,
    completed: Arc<AtomicUsize>,
}

const _: () = core::assert!(core::mem::offset_of!(BudgetProbe, node) == 0);

/// # Safety
/// `node` is the first-field projection of a uniquely submitted BudgetProbe
/// Box. Invocation consumes that allocation exactly once after its GP.
unsafe fn finish_budget_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the submitted Box base.
    let probe = unsafe { Box::from_raw(node.cast::<BudgetProbe>()) };
    probe.completed.fetch_add(1, Ordering::Relaxed);
}

#[def_test(serial)]
fn callback_batches_rearm_only_for_ready_budget_remainder() {
    static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    let completed = Arc::new(AtomicUsize::new(0));
    let probes: alloc::vec::Vec<_> = (0..2 * CALLBACK_BUDGET + 1)
        .map(|_| {
            Box::new(BudgetProbe {
                node: CallbackNode::new(finish_budget_probe),
                completed: completed.clone(),
            })
        })
        .collect();
    let _guard = kspin::NoPreemptIrqSave::new();
    let cpu = RcuExecutionIf::current_cpu().as_usize();
    // SAFETY: this serial fixture alone coordinates its static nodes, enrolls
    // no readers and drains all unpublished probes before releasing the state.
    let callbacks =
        unsafe { CallbackState::new(&NODES, core::array::from_fn(|id| id == cpu)) }.unwrap();
    let service = GracePeriodTask {
        drain: drain::CallbackDrain::new(),
        callbacks,
        has_started: kspin::SpinNoPreempt::new(false),
        waker: Once::new(),
        has_demand: AtomicBool::new(false),
    };
    let mut driver = service.callbacks.claim_driver().unwrap();
    for probe in probes {
        let raw = Box::into_raw(probe);
        // SAFETY: field projection preserves the original Box provenance.
        let node = unsafe { &raw mut (*raw).node };
        // SAFETY: uniquely owned unpublished probe; nonblocking callback
        // destroys it once, and this fixture supplies all execution/progress.
        let accepted = unsafe { service.callbacks.submit(cpu, node) };
        if !accepted {
            // SAFETY: rejected admission leaves the original Box caller-owned.
            drop(unsafe { Box::from_raw(raw) });
        }
        assert!(accepted);
    }
    assert!(driver.advance());
    let mut notifications = 0;
    // SAFETY: sole pinned executor, no bookkeeping locks or real retired data;
    // probes only count and release storage, and notification only counts.
    unsafe { service.invoke_callback_batch(cpu, || notifications += 1) };
    assert_eq!(completed.load(Ordering::Relaxed), 0);
    assert_eq!(notifications, 0);
    let was_reading = crate::host::with_current_reader(|current| {
        if current.reader().is_reading() {
            return true;
        }
        // SAFETY: retained current at an IRQ/preemption-excluded boundary;
        // this isolated period has no real protected data or enrolled readers.
        unsafe { service.callbacks.note_context_switch(cpu, current.reader()) };
        false
    });
    assert!(!was_reading);
    assert!(!driver.advance());
    for (expected, expected_notifications) in [
        (CALLBACK_BUDGET, 1),
        (2 * CALLBACK_BUDGET, 2),
        (2 * CALLBACK_BUDGET + 1, 2),
        (2 * CALLBACK_BUDGET + 1, 2),
    ] {
        // SAFETY: same sole executor and nonblocking probes; the isolated GP
        // is complete, and notification does not recursively run callbacks.
        unsafe { service.invoke_callback_batch(cpu, || notifications += 1) };
        assert_eq!(completed.load(Ordering::Relaxed), expected);
        assert_eq!(notifications, expected_notifications);
    }
    assert!(!service.callbacks.has_ready(cpu));
    assert_eq!(Arc::strong_count(&completed), 1);
}

#[def_test]
fn action_conflict_preserves_control_for_startup_retry() {
    // These isolated nodes have static storage but no GP is started and no
    // reader is enrolled. The fixture never touches the live boot service.
    static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    // SAFETY: this fixture is the sole coordinator of these inactive nodes.
    let callbacks =
        unsafe { CallbackState::new(&NODES, core::array::from_fn(|cpu| cpu == 0)) }.unwrap();
    let service = GracePeriodTask {
        drain: drain::CallbackDrain::new(),
        callbacks,
        has_started: kspin::SpinNoPreempt::new(false),
        waker: Once::new(),
        has_demand: AtomicBool::new(false),
    };
    // SAFETY: no startup waker exists. Admission must reject before inspecting
    // the deliberately absent node or claiming any callback ownership.
    let rejected = unsafe { service.submit_callback(core::ptr::null_mut()) };
    assert_eq!(rejected, Err(KError::BadState));
    assert!(!service.has_demand.load(Ordering::Acquire));
    assert!(matches!(
        service.claim_driver_with_action(|| false),
        Err(KError::ResourceBusy)
    ));
    assert!(!*service.has_started.lock());
    {
        let _driver = service.claim_driver_with_action(|| true).unwrap();
        assert!(*service.has_started.lock());
        assert!(service.callbacks.claim_driver().is_none());
        let mut was_installer_called = false;
        assert!(matches!(
            service.claim_driver_with_action(|| {
                was_installer_called = true;
                true
            }),
            Err(KError::BadState)
        ));
        assert!(!was_installer_called);
    }
    assert!(matches!(
        service.claim_driver_with_action(|| false),
        Err(KError::BadState)
    ));
}

#[def_test(serial)]
fn callback_preflight_checks_isolated_readiness_without_claiming_control() {
    // This service never starts a task or installs an action. Its private
    // readiness publication models startup ordering without resetting SERVICE.
    static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    // SAFETY: this serial fixture alone coordinates its inactive static nodes.
    // No reader is enrolled and no callback is submitted.
    let callbacks = unsafe {
        CallbackState::new(
            &NODES,
            core::array::from_fn(|cpu| cpu < kcpu_id_map::nr_cpus()),
        )
    }
    .unwrap();
    let service = GracePeriodTask {
        drain: drain::CallbackDrain::new(),
        callbacks,
        has_started: kspin::SpinNoPreempt::new(false),
        waker: Once::new(),
        has_demand: AtomicBool::new(false),
    };
    let mut publication = 0;
    let unclaimed = service.preflight_callback_submission();
    if unclaimed.is_ok() {
        publication = 1;
    }
    assert_eq!(unclaimed, Err(KError::BadState));
    assert_eq!(publication, 0);
    assert!(!*service.has_started.lock());
    assert!(service.waker.get().is_none());
    assert!(!service.has_demand.load(Ordering::Acquire));

    let _driver = service.claim_driver_with_action(|| true).unwrap();
    let unprepared = service.preflight_callback_submission();
    if unprepared.is_ok() {
        publication = 1;
    }
    assert_eq!(unprepared, Err(KError::BadState));
    assert_eq!(publication, 0);
    // Publish only after claiming the isolated driver. The no-op waker is not
    // attached to any task and this fixture never asks the service to run.
    service.waker.call_once(|| Waker::noop().clone());
    assert_eq!(service.preflight_callback_submission(), Ok(()));
    assert!(karch::local_irq_enabled());
    assert!(!RcuTaskIf::current_preemption_disabled());

    let (task_preflight, callback_readiness) = {
        let _irq = kspin::IrqSave::new();
        (
            service.preflight_callback_submission(),
            service.check_callback_readiness(),
        )
    };
    assert_eq!(task_preflight, Err(KError::InvalidInput));
    // Shared readiness must not impose task-only restrictions on submit.
    assert_eq!(callback_readiness, Ok(()));
    let unwatched = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let watching = krcu_core::RcuContextIf::current_cpu_watching();
        // SAFETY: current CPU is pinned with IRQs excluded and no active read
        // scope. Only the readiness query runs before watching is restored.
        unsafe { watching.enter_quiescent() };
        let result = service.check_callback_readiness();
        // SAFETY: matching same-CPU transition without scheduling or read access.
        unsafe { watching.exit_quiescent() };
        result
    };
    assert_eq!(unwatched, Err(KError::InvalidInput));
    assert_eq!(service.preflight_callback_submission(), Ok(()));
    assert!(service.callbacks.claim_driver().is_none());
    assert!(!service.has_demand.load(Ordering::Acquire));
    assert_eq!(publication, 0);
}

#[def_test(serial)]
fn boot_service_retains_prepared_waker_and_unique_driver() {
    let service = SERVICE.get().unwrap();
    assert!(service.waker.get().is_some());
    assert!(*service.has_started.lock());
    assert!(service.callbacks.claim_driver().is_none());
}

#[def_test(serial)]
fn callback_admission_rejects_unwatched_before_accessing_node() {
    let service = SERVICE.get().unwrap();
    let _guard = kspin::NoPreemptIrqSave::new();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    // SAFETY: this CPU is pinned with IRQs excluded and no active read scope.
    // Only the admission rejection runs before the paired restoration below.
    unsafe { watching.enter_quiescent() };
    // SAFETY: unwatched admission rejects without examining the absent node.
    let result = unsafe { service.submit_callback(core::ptr::null_mut()) };
    // SAFETY: same CPU/exclusion, no intervening scheduling or protected access.
    unsafe { watching.exit_quiescent() };
    assert_eq!(result, Err(KError::InvalidInput));
}

#[def_test]
fn progress_deadlines_remain_in_timer_range() {
    assert_eq!(
        progress_rearm_deadline(0, None, 0),
        Some(PROGRESS_INTERVAL_NS)
    );
    assert_eq!(
        progress_rearm_deadline(12, None, 12),
        Some(12 + PROGRESS_INTERVAL_NS)
    );
    assert_eq!(
        progress_rearm_deadline(u64::MAX - 1, None, u64::MAX - 1),
        Some(u64::MAX)
    );
    assert_eq!(
        progress_rearm_deadline(u64::MAX, None, u64::MAX),
        Some(u64::MAX)
    );
}

#[def_test]
fn repeated_demand_does_not_postpone_progress_deadline() {
    let deadline = progress_rearm_deadline(0, None, 0).unwrap();
    // Each call models a demanded pass while the GP remains active. No wall
    // clock or scheduling assumption is needed to test the production decision.
    for now in [0, 1, deadline / 2, deadline - 1] {
        assert_eq!(progress_rearm_deadline(now, Some(deadline), now), None);
    }
    assert_eq!(
        progress_rearm_deadline(deadline, Some(deadline), deadline),
        Some(deadline + PROGRESS_INTERVAL_NS)
    );
    assert_eq!(
        progress_rearm_deadline(deadline + 1, Some(deadline), deadline + 1),
        Some(deadline + 1 + PROGRESS_INTERVAL_NS)
    );
}

#[def_test]
fn slow_progress_round_rearms_from_completion_time() {
    let interval = PROGRESS_INTERVAL_NS;
    let finished_ns = 4 * interval;
    // Both initial assistance and a due round can take longer than an interval.
    assert_eq!(
        progress_rearm_deadline(0, None, finished_ns),
        Some(5 * interval)
    );
    assert_eq!(
        progress_rearm_deadline(interval, Some(interval), finished_ns),
        Some(5 * interval)
    );
    // An early demanded pass performed no timed assistance. Even if its work
    // crossed the old deadline, preserve that obligation for the next pass.
    assert_eq!(
        progress_rearm_deadline(interval - 1, Some(interval), finished_ns),
        None
    );
}

#[repr(C)]
struct ProgressCallbackProbe {
    node: CallbackNode,
    completed: Arc<AtomicUsize>,
    violations: Arc<AtomicUsize>,
    cpu: usize,
    completion_waker: Waker,
}

const _: () = core::assert!(core::mem::offset_of!(ProgressCallbackProbe, node) == 0);

/// # Safety
/// The node is the first-field projection of a uniquely submitted probe Box.
/// The executor has detached it after its GP and transfers ownership once.
unsafe fn finish_progress_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the submitted Box base.
    let probe = unsafe { Box::from_raw(node.cast::<ProgressCallbackProbe>()) };
    // Runtime's callback tests verify the specific softirq carrier. This
    // internal fixture checks its host-visible execution and reader contracts.
    let is_valid_context = RcuExecutionIf::is_in_interrupt_handler()
        && !RcuExecutionIf::is_in_nmi()
        && RcuTaskIf::current_preemption_disabled();
    if !is_valid_context || RcuExecutionIf::current_cpu().as_usize() != probe.cpu {
        probe.violations.fetch_add(1, Ordering::Relaxed);
    }
    crate::host::with_current_reader(|current| {
        let reader = current.reader();
        let cpu = RcuExecutionIf::current_cpu();
        let before = (
            reader.is_reading(),
            reader.is_enrolled(),
            crate::context::needs_reader_cleanup(cpu),
            karch::local_irq_enabled(),
        );
        // The real carrier pins current. No additional exclusion may hide a
        // dispatch defect; nested interrupts must return before this resumes.
        let scopes_were_active = crate::with_interrupt_read(|_| {
            let inner_was_active = crate::with_interrupt_read(|_| {
                reader.is_reading()
                    && RcuExecutionIf::is_in_interrupt_handler()
                    && !RcuExecutionIf::is_in_nmi()
                    && RcuTaskIf::current_preemption_disabled()
                    && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
            });
            inner_was_active && reader.is_reading()
        });
        let after = (
            reader.is_reading(),
            reader.is_enrolled(),
            crate::context::needs_reader_cleanup(cpu),
            karch::local_irq_enabled(),
        );
        if !scopes_were_active || before != after {
            probe.violations.fetch_add(1, Ordering::Relaxed);
        }
    });
    probe.completed.fetch_add(1, Ordering::Release);
    probe.completion_waker.wake_by_ref();
    // Completion does not transfer storage back to the waiter. This callback
    // retains its Box and Arc resources until return, including through wakeup.
}

fn submit_progress_probe(
    service: &GracePeriodTask,
    cpu: usize,
    completed: &Arc<AtomicUsize>,
    violations: &Arc<AtomicUsize>,
    completion_waker: Waker,
) -> bool {
    let object = Box::into_raw(Box::new(ProgressCallbackProbe {
        node: CallbackNode::new(finish_progress_probe),
        completed: completed.clone(),
        violations: violations.clone(),
        cpu,
        completion_waker,
    }));
    // SAFETY: raw projection preserves the original enclosing Box provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: boot retains this participating CPU's queue and carrier. The
    // unpublished probe has no protected readers; invocation consumes its
    // unique allocation nonblockingly, with transferable Arc/waker resources.
    if unsafe { service.callbacks.submit(cpu, node) } {
        true
    } else {
        // SAFETY: rejection leaves the original allocation uniquely caller-owned.
        drop(unsafe { Box::from_raw(object) });
        false
    }
}

#[def_test(serial)]
fn callback_progress_does_not_require_yielding_test_workers() {
    let service = SERVICE.get().unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let accepted = crate::host::with_prepared_service_wait(|wait| {
        let mut accepted = 0;
        for _ in 0..4 {
            for cpu in 0..kcpu_id_map::nr_cpus() {
                accepted += usize::from(submit_progress_probe(
                    service,
                    cpu,
                    &completed,
                    &violations,
                    wait.waker(),
                ));
            }
            service.request_pass();
            // No producer/observer task is created or joined to supply CPU
            // observations. The submitter parks; retained wakes cover callback
            // completion before park. Other interrupts are not excluded.
            while completed.load(Ordering::Acquire) < accepted {
                wait.park();
            }
        }
        accepted
    })
    .unwrap();
    assert_eq!(accepted, 4 * kcpu_id_map::nr_cpus());
    assert_eq!(completed.load(Ordering::Acquire), accepted);
    assert_eq!(violations.load(Ordering::Relaxed), 0);
}

// The host's IPI provider includes boot-time self-notification setup.
#[cfg(feature = "ipi")]
#[def_test(serial)]
fn cleanup_demand_kicks_an_already_observed_cpu() {
    use alloc::boxed::Box;

    use krcu_core::CallbackNode;

    /// # Safety
    /// `node` is a uniquely owned Box<CallbackNode>, submitted once and detached
    /// after its period. Invocation consumes that original allocation once.
    unsafe fn release_marker(node: *mut CallbackNode) {
        // SAFETY: submission transferred this Box allocation unchanged.
        drop(unsafe { Box::from_raw(node) });
    }

    let (
        accepted,
        started,
        no_cpu_obligation,
        retained,
        kicked,
        demand_retained,
        cleaned_by_boundary,
        is_active,
        invoked,
    ) = crate::host::with_current_reader(|current| {
        static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
            [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
        let irq = kspin::IrqSave::new();
        let cpu = RcuExecutionIf::current_cpu().as_usize();
        // SAFETY: this serial fixture exclusively controls its static nodes and
        // completes all callbacks before returning. No real object is retired.
        let callbacks =
            unsafe { CallbackState::new(&NODES, core::array::from_fn(|id| id == cpu)) }.unwrap();
        let service = GracePeriodTask {
            drain: drain::CallbackDrain::new(),
            callbacks,
            has_started: kspin::SpinNoPreempt::new(false),
            waker: Once::new(),
            has_demand: AtomicBool::new(false),
        };
        let mut driver = service.callbacks.claim_driver().unwrap();
        let pin = kspin::NoPreempt::new();
        let marker = Box::into_raw(Box::new(CallbackNode::new(release_marker)));
        // SAFETY: this isolated marker protects no data. The driver owns it until
        // invoking its nonblocking release callback; rejection retains ownership.
        let accepted = unsafe { service.callbacks.submit(cpu, marker) };
        if !accepted {
            // SAFETY: rejected submission neither accesses nor retains the marker.
            drop(unsafe { Box::from_raw(marker) });
        }
        let started = driver.advance();
        crate::read::with_task_read(|_| {
            // SAFETY: explicit outgoing-current fixture under IRQ/preemption
            // exclusion, static nodes; no real retired data uses this observation.
            unsafe { service.callbacks.note_context_switch(cpu, current.reader()) };
        });
        let no_cpu_obligation = !service.callbacks.needs_cpu_observation(cpu);
        let retained = current.reader().is_enrolled();
        let kicked = service.request_cpu_progress(cpu);
        let demand_retained =
            crate::context::needs_reader_cleanup(kcpu_id_map::LogicalCpuId::new(cpu));
        // Suppress the pin's cleanup probe to leave the IRQ-only deferred case.
        crate::host::with_preempt_check_deferred(|| drop(pin));
        drop(irq);
        let start_ns = RcuTimerIf::monotonic_time_nanos();
        while current.reader().is_enrolled()
            && RcuTimerIf::monotonic_time_nanos().saturating_sub(start_ns) < 5_000_000_000
        {
            core::hint::spin_loop();
        }
        let cleaned_by_boundary = !current.reader().is_enrolled();
        // Independently drain the fixture even if the tested delivery regresses.
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: retained current task at an excluded watched task boundary.
        unsafe { crate::context::finish_reader_cleanup(current.reader()) };
        // SAFETY: fresh ready check, no locks held, isolated fixture only.
        unsafe { service.callbacks.report_ready(cpu) };
        let is_active = driver.advance();
        // SAFETY: the controlled period is complete; the retained marker only
        // releases its allocation. No bookkeeping lock is held. Unlike the live
        // service probe, it needs no softirq context or interrupt read admission.
        let invoked = unsafe { service.callbacks.invoke_ready(cpu, 1) };
        (
            accepted,
            started,
            no_cpu_obligation,
            retained,
            kicked,
            demand_retained,
            cleaned_by_boundary,
            is_active,
            invoked,
        )
    });
    assert!(accepted && started && no_cpu_obligation && retained && kicked && demand_retained);
    assert!(cleaned_by_boundary);
    assert!(!is_active);
    assert_eq!(invoked, 1);
}
