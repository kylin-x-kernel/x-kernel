// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-owned RCU GP task integration. The task and its state intentionally persist
//! for the kernel lifetime; these tests neither replace nor reinitialize them.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::AtomicUsize;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu_core::CallbackNode;
use unittest::{assert, assert_eq, def_test};

use super::*;
use ktask::TaskInner;

#[def_test]
fn action_conflict_preserves_control_for_startup_retry() {
    // These isolated nodes have static storage but no GP is started and no
    // reader is enrolled. The fixture never touches the live boot service.
    static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    // SAFETY: this fixture is the sole coordinator of these inactive nodes.
    let callbacks = unsafe { CallbackState::new(&NODES, core::array::from_fn(|cpu| cpu == 0)) }
        .unwrap();
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
fn gp_boot_prepares_notification_and_claims_control_once() {
    let service = SERVICE.get().unwrap();
    assert!(service.waker.get().is_some());
    assert!(service.callbacks.claim_driver().is_none());
    // SAFETY: boot completed the CPU-service rendezvous. The duplicate call
    // exercises rejection before it creates another task or resets any state.
    let result = unsafe { init_service() };
    assert_eq!(result, Err(KError::BadState));
}

#[def_test(serial)]
fn callback_admission_rejects_nmi_before_accessing_node() {
    let service = SERVICE.get().unwrap();
    let _guard = kspin::NoPreemptIrqSave::new();
    let reject_fn = || {
        // SAFETY: NMI admission rejects before examining the absent node.
        unsafe { service.submit_callback(core::ptr::null_mut()) }
    };
    // SAFETY: this controlled NMI is strictly nested under local exclusion.
    // Its closure only exercises rejection and never acquires a queue lock.
    let result = unsafe { kirq::context::test_support::with_nmi_context(reject_fn) };
    assert_eq!(result, Err(KError::InvalidInput));
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

#[def_test(serial)]
fn gp_task_consumes_repeated_retained_demand() {
    let service = SERVICE.get().unwrap();
    // No ordinary RCU producers are admitted yet. Each request is a genuine
    // pass through the boot-created task, not a test-side clear of its flag.
    for _ in 0..8 {
        service.request_pass();
        while service.has_demand.load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
    assert!(service.waker.get().is_some());
}

#[def_test]
fn progress_deadlines_remain_in_timer_range() {
    assert_eq!(progress_rearm_deadline(0, None, 0), Some(PROGRESS_INTERVAL_NS));
    assert_eq!(
        progress_rearm_deadline(12, None, 12),
        Some(12 + PROGRESS_INTERVAL_NS)
    );
    assert_eq!(progress_rearm_deadline(u64::MAX - 1, None, u64::MAX - 1), Some(u64::MAX));
    assert_eq!(progress_rearm_deadline(u64::MAX, None, u64::MAX), Some(u64::MAX));
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
    assert_eq!(progress_rearm_deadline(0, None, finished_ns), Some(5 * interval));
    assert_eq!(
        progress_rearm_deadline(interval, Some(interval), finished_ns),
        Some(5 * interval)
    );
    // An early demanded pass performed no timed assistance. Even if its work
    // crossed the old deadline, preserve that obligation for the next pass.
    assert_eq!(progress_rearm_deadline(interval - 1, Some(interval), finished_ns), None);
}

#[repr(C)]
struct CallbackProbe {
    node: CallbackNode,
    completed: Arc<AtomicUsize>,
    violations: Arc<AtomicUsize>,
    cpu: usize,
    completion_waker: Option<Waker>,
}

/// # Safety
/// `node` is the first field of a uniquely owned CallbackProbe Box, submitted
/// once and detached after its GP. Invocation takes ownership of that allocation.
unsafe fn finish_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C) puts node at offset zero; admission passed a raw projection
    // of this original Box allocation, and execution grants unique ownership.
    let probe = unsafe { Box::from_raw(node.cast::<CallbackProbe>()) };
    if !kirq::context::is_softirq_context()
        || ktask::is_sleepable_context()
        || khal::percpu::this_cpu_id().as_usize() != probe.cpu
    {
        probe.violations.fetch_add(1, Ordering::Relaxed);
    }
    // Exercise admission at the real callback endpoint, without installing a
    // synthetic context or adding exclusion that could hide a dispatch defect.
    // Softirq pins current; nested IRQs must return before this callback resumes.
    let task = ktask::current();
    let cpu = khal::percpu::this_cpu_id();
    let before = (
        crate::test_support::reader(&task).is_reading(),
        crate::test_support::reader(&task).is_enrolled(),
        crate::context::needs_reader_cleanup(cpu),
        karch::local_irq_enabled(),
    );
    let scopes_were_active = crate::with_interrupt_read(|_| {
        let inner_was_active = crate::with_interrupt_read(|_| {
            crate::test_support::reader(&task).is_reading()
                && kirq::context::is_softirq_context()
                && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
        });
        inner_was_active && crate::test_support::reader(&task).is_reading()
    });
    let after = (
        crate::test_support::reader(&task).is_reading(),
        crate::test_support::reader(&task).is_enrolled(),
        crate::context::needs_reader_cleanup(cpu),
        karch::local_irq_enabled(),
    );
    if !scopes_were_active || before != after {
        probe.violations.fetch_add(1, Ordering::Relaxed);
    }
    probe.completed.fetch_add(1, Ordering::Release);
    if let Some(waker) = &probe.completion_waker {
        waker.wake_by_ref();
    }
    // The callback owns its allocation through return. Observing completed does
    // not let the test free callback storage or assume queue accounting is done.
}

fn submit_probe(
    service: &GracePeriodTask,
    cpu: usize,
    completed: &Arc<AtomicUsize>,
    violations: &Arc<AtomicUsize>,
    completion_waker: Option<Waker>,
) -> bool {
    let object = Box::into_raw(Box::new(CallbackProbe {
        node: CallbackNode::new(finish_probe),
        completed: completed.clone(),
        violations: violations.clone(),
        cpu,
        completion_waker,
    }));
    // SAFETY: raw field projection preserves the containing Box provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: boot prepared the actual service. This unpublished object has no
    // protected readers; its nonblocking callback owns the retained allocation
    // and Arc resources, all transferable across CPUs, until execution finishes.
    if unsafe { service.callbacks.submit(cpu, node) } {
        true
    } else {
        // SAFETY: rejection does not access or accept the node, leaving this
        // unique original allocation owned by the fixture.
        drop(unsafe { Box::from_raw(object) });
        false
    }
}

#[def_test(serial)]
fn gp_task_delivers_scoped_callbacks_through_each_cpus_softirq() {
    let service = SERVICE.get().unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let is_finished = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    // This fixture supplies real scheduler boundaries on every participant.
    // It does not simulate observations or claim idle/no-tick progress coverage.
    for cpu in 0..kcpu_id_map::nr_cpus() {
        let finished = is_finished.clone();
        let worker = ktask::prepare_task(TaskInner::new_pidless_kthread(
            move || {
                while !finished.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
            },
            "rcu-observer-test".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        worker.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu)));
        ktask::activate_task(&worker);
        workers.push(worker);
    }
    let per_cpu = CALLBACK_BUDGET * 2 + 1;
    let mut accepted = 0;
    for _ in 0..2 {
        for cpu in 0..kcpu_id_map::nr_cpus() {
            for _ in 0..per_cpu {
                accepted += usize::from(submit_probe(service, cpu, &completed, &violations, None));
            }
        }
        service.request_pass();
        // Only the real callback endpoint advances completed. The bounded QEMU
        // run detects loss of service; this is not a wall-clock latency test.
        while completed.load(Ordering::Acquire) < accepted {
            ktask::yield_now();
        }
    }
    is_finished.store(true, Ordering::Release);
    let mut exited_cleanly = true;
    for worker in workers {
        exited_cleanly &= worker.join() == 0;
    }
    assert!(exited_cleanly);
    assert_eq!(accepted, 2 * kcpu_id_map::nr_cpus() * per_cpu);
    assert_eq!(completed.load(Ordering::Acquire), accepted);
    assert_eq!(violations.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn callback_progress_does_not_require_yielding_test_workers() {
    let service = SERVICE.get().unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let mut accepted = 0;
    for _ in 0..4 {
        for cpu in 0..kcpu_id_map::nr_cpus() {
            accepted += usize::from(submit_probe(
                service,
                cpu,
                &completed,
                &violations,
                Some(wait.waker()),
            ));
        }
        service.request_pass();
        // No helper task supplies scheduler observations. The submitter sleeps;
        // retained endpoint wakes also cover completion before the actual park.
        // This does not assert that every other source of interrupts is absent.
        while completed.load(Ordering::Acquire) < accepted {
            wait.park();
        }
    }
    assert_eq!(accepted, 4 * kcpu_id_map::nr_cpus());
    assert_eq!(completed.load(Ordering::Acquire), accepted);
    assert_eq!(violations.load(Ordering::Relaxed), 0);
}

#[derive(Default)]
struct DeferredReaderProbe {
    has_entered: AtomicBool,
    has_reached_destination: AtomicBool,
    can_exit: AtomicBool,
    has_retained_cleanup: AtomicBool,
    was_cleaned_at_boundary: AtomicBool,
}

#[def_test(serial)]
fn deferred_reader_exit_completes_through_live_gp_and_softirq() {
    assert_eq!(verify_deferred_reader_completion(LogicalCpuId::new(0)), unittest::TestResult::Ok);
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn migrated_deferred_reader_cleans_original_node_from_destination() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    assert_eq!(verify_deferred_reader_completion(LogicalCpuId::new(1)), unittest::TestResult::Ok);
}

fn verify_deferred_reader_completion(destination: LogicalCpuId) -> unittest::TestResult {
    let service = SERVICE.get().unwrap();
    let probe = Arc::new(DeferredReaderProbe::default());
    let reader_probe = probe.clone();
    let reader = ktask::prepare_task(TaskInner::new_pidless_kthread(
        move || {
            let task = ktask::current();
            let irq = crate::read::with_task_read(|_| {
                reader_probe.has_entered.store(true, Ordering::Release);
                while !reader_probe.can_exit.load(Ordering::Acquire) {
                    if khal::percpu::this_cpu_id() == destination {
                        reader_probe.has_reached_destination.store(true, Ordering::Release);
                    }
                    core::hint::spin_loop();
                }
                // Return the exclusion guard, not the read token. The outer
                // scope exit must retain membership until an allowed boundary.
                kspin::IrqSave::new()
            });
            let cpu = khal::percpu::this_cpu_id();
            reader_probe.has_retained_cleanup.store(
                !crate::test_support::reader(&task).is_reading()
                    && crate::test_support::reader(&task).is_enrolled()
                    && cpu == destination
                    && crate::context::needs_reader_cleanup(destination),
                Ordering::Release,
            );
            drop(irq);
            let start_ns = khal::time::monotonic_time_nanos();
            while crate::test_support::reader(&task).is_enrolled()
                && khal::time::monotonic_time_nanos().saturating_sub(start_ns) < 5_000_000_000
            {
                core::hint::spin_loop();
            }
            reader_probe
                .was_cleaned_at_boundary
                .store(!crate::test_support::reader(&task).is_enrolled(), Ordering::Release);
            // Independent fallback before returning the task: a failed delivery
            // assertion must not strand membership in the live service.
            ktask::check_preempt_pending();
        },
        "rcu-deferred-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    let cpu = LogicalCpuId::new(0);
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
    ktask::activate_task(&reader);
    while !probe.has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    // The reader never blocks or yields in its scope. Joining a same-CPU
    // competitor proves an actual switch enrolled it, independent of whether
    // a reschedule request alone would have selected another task.
    let competitor = ktask::prepare_task(TaskInner::new_pidless_kthread(
        || {},
        "rcu-deferred-competitor".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    competitor.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
    ktask::activate_task(&competitor);
    let competitor_status = competitor.join();
    let was_enrolled = crate::test_support::reader(&reader).is_enrolled();
    let affinity_applied = if destination != cpu {
        ktask::set_task_affinity(&reader, ktask::KCpuMask::one_shot_logical(destination))
    } else {
        true
    };
    if affinity_applied {
        // The running reader acknowledges the CPU, not merely a changed
        // affinity field. Its active scope must retain the original owner.
        while !probe.has_reached_destination.load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
    let retains_source = crate::test_support::reader(&reader).is_enrolled_on(crate::context::reader_node(cpu));
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let accepted = submit_probe(
        service,
        destination.as_usize(),
        &completed,
        &violations,
        Some(wait.waker()),
    );
    service.request_pass();
    probe.can_exit.store(true, Ordering::Release);
    let reader_status = reader.join();
    // Only the boot GP task advances the period and only its real softirq
    // invokes this callback. No test-side observation, driver pass or cleanup
    // report substitutes for those runtime paths. Other IRQs may assist the
    // reader's exit: this is not proof that an RCU IPI was the unique cause.
    while accepted && completed.load(Ordering::Acquire) == 0 {
        wait.park();
    }
    assert_eq!(competitor_status, 0);
    assert_eq!(reader_status, 0);
    assert!(affinity_applied);
    assert!(probe.has_reached_destination.load(Ordering::Acquire));
    assert!(was_enrolled);
    assert!(retains_source);
    assert!(accepted);
    assert!(probe.has_retained_cleanup.load(Ordering::Acquire));
    assert!(probe.was_cleaned_at_boundary.load(Ordering::Acquire));
    assert!(!crate::test_support::reader(&reader).is_enrolled());
    assert_eq!(completed.load(Ordering::Acquire), 1);
    assert_eq!(violations.load(Ordering::Relaxed), 0);
    unittest::TestResult::Ok
}

// ktask/ipi includes boot-time fixed notification setup, including self IPIs.
#[cfg(feature = "ipi")]
#[def_test(serial)]
fn cleanup_demand_kicks_an_already_observed_cpu() {
    /// # Safety
    /// `node` is a uniquely owned Box<CallbackNode>, submitted once and detached
    /// after its period. Invocation consumes that original allocation once.
    unsafe fn release_marker(node: *mut CallbackNode) {
        // SAFETY: submission transferred this Box allocation unchanged.
        drop(unsafe { Box::from_raw(node) });
    }

    static NODES: [krcu_core::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    let task = ktask::current();
    let irq = kspin::IrqSave::new();
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
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
        unsafe { service.callbacks.note_context_switch(cpu, crate::test_support::reader(&task)) };
    });
    let no_cpu_obligation = !service.callbacks.needs_cpu_observation(cpu);
    let retained = crate::test_support::reader(&task).is_enrolled();
    let kicked = service.request_cpu_progress(cpu);
    let demand_retained = crate::context::needs_reader_cleanup(LogicalCpuId::new(cpu));
    // Suppress the pin's cleanup probe to leave the IRQ-only deferred case.
    ktask::with_preempt_check_deferred(|| drop(pin));
    drop(irq);
    let start_ns = khal::time::monotonic_time_nanos();
    while crate::test_support::reader(&task).is_enrolled()
        && khal::time::monotonic_time_nanos().saturating_sub(start_ns) < 5_000_000_000
    {
        core::hint::spin_loop();
    }
    let cleaned_by_boundary = !crate::test_support::reader(&task).is_enrolled();
    // Independently drain the fixture even if the tested delivery regresses.
    let _guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: retained current task at an excluded watched task boundary.
    unsafe { crate::context::finish_reader_cleanup(crate::test_support::reader(&task)) };
    // SAFETY: fresh ready check, no locks held, isolated fixture only.
    unsafe { service.callbacks.report_ready(cpu) };
    let is_active = driver.advance();
    // SAFETY: the controlled period is complete; the retained marker only
    // releases its allocation. No bookkeeping lock is held. Unlike the live
    // service probe, it needs no softirq context or interrupt read admission.
    let invoked = unsafe { service.callbacks.invoke_ready(cpu, 1) };
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(accepted && started && no_cpu_obligation && retained && kicked && demand_retained);
    assert!(cleaned_by_boundary);
    assert!(!is_active);
    assert_eq!(invoked, 1);
}
