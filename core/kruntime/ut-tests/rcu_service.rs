// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-owned RCU service integration through public owning callbacks.
//! The permanent service is neither replaced nor reinitialized by these tests.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kerrno::KError;
use krcu::{
    CallbackNode, PreparedCallback,
    host::{RcuSoftirqIf, RcuTimerIf},
};
use ktask::{TaskInner, future::PreparedTaskWait};
use unittest::{assert, assert_eq, def_test};

use super::rcu_support::reader as task_reader;

// Workload size, not a copy of the private executor budget.
const CALLBACKS_PER_CPU: usize = 97;

#[def_test(serial)]
fn gp_boot_rejects_duplicate_control_claim() {
    // SAFETY: boot completed the CPU-service rendezvous. The duplicate call
    // exercises rejection before it creates another task or resets any state.
    let result = unsafe { krcu::init_service() };
    assert_eq!(result, Err(KError::BadState));
}

#[def_test(serial)]
fn gp_startup_rejects_masked_caller_before_duplicate_control_check() {
    let irq = kspin::IrqSave::new();
    // SAFETY: boot completed the CPU-service rendezvous. Invalid waiting
    // context must be rejected before inspecting or claiming service control.
    let result = unsafe { krcu::init_service() };
    drop(irq);
    assert_eq!(result, Err(KError::InvalidInput));
}

#[def_test(serial)]
fn progress_timer_adapter_delivers_without_inline_execution() {
    static DELIVERIES: AtomicUsize = AtomicUsize::new(0);
    fn notify_fn() {
        DELIVERIES.fetch_add(1, Ordering::Release);
    }

    DELIVERIES.store(0, Ordering::Release);
    let mut observations = None;
    RcuTimerIf::with_progress_timer(notify_fn, &mut |timer| {
        let irq = kspin::IrqSave::new();
        // SAFETY: boot prepared all timer owners; this fixture's handler only
        // increments static storage and never waits or reenters timer dispatch.
        let armed = unsafe { timer.arm(RcuTimerIf::monotonic_time_nanos()) };
        let before_irq_restore = DELIVERIES.load(Ordering::Acquire);
        drop(irq);
        let started_ns = RcuTimerIf::monotonic_time_nanos();
        while armed.is_ok()
            && DELIVERIES.load(Ordering::Acquire) == 0
            && RcuTimerIf::monotonic_time_nanos().saturating_sub(started_ns) < 5_000_000_000
        {
            ktask::yield_now();
        }
        timer.cancel_sync();
        let after_delivery = DELIVERIES.load(Ordering::Acquire);
        // SAFETY: same initialized service and IRQ-safe static handler; rearm
        // reuses the owned timer and cancellation excludes any later delivery.
        let rearmed = unsafe { timer.arm(u64::MAX) };
        timer.cancel_sync();
        observations = Some((armed, before_irq_restore, after_delivery, rearmed));
    });
    let (armed, before_irq_restore, after_delivery, rearmed) = observations.unwrap();
    assert_eq!(armed, Ok(()));
    assert_eq!(before_irq_restore, 0);
    assert_eq!(after_delivery, 1);
    assert_eq!(rearmed, Ok(()));
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 1);
}

#[repr(C)]
struct CallbackProbe {
    node: CallbackNode,
    completed: Arc<AtomicUsize>,
    violations: Arc<AtomicUsize>,
    cpu: usize,
    completion_waker: Option<Waker>,
}

const _: () = core::assert!(core::mem::offset_of!(CallbackProbe, node) == 0);

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
        task_reader(&task).is_reading(),
        task_reader(&task).is_enrolled(),
        krcu::reader_cleanup_requested_for_tests(cpu),
        karch::local_irq_enabled(),
    );
    let scopes_were_active = krcu::with_interrupt_read(|_| {
        let inner_was_active = krcu::with_interrupt_read(|_| {
            task_reader(&task).is_reading()
                && kirq::context::is_softirq_context()
                && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
        });
        inner_was_active && task_reader(&task).is_reading()
    });
    let after = (
        task_reader(&task).is_reading(),
        task_reader(&task).is_enrolled(),
        krcu::reader_cleanup_requested_for_tests(cpu),
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

/// # Safety
/// The node is an unsubmitted CallbackProbe allocation owned by its handle.
unsafe fn cancel_probe(node: *mut CallbackNode) {
    // SAFETY: the prepared handle retains the original first-field projection.
    drop(unsafe { Box::from_raw(node.cast::<CallbackProbe>()) });
}

fn prepare_probe(
    cpu: usize,
    completed: &Arc<AtomicUsize>,
    violations: &Arc<AtomicUsize>,
    completion_waker: Option<Waker>,
) -> PreparedCallback {
    let object = Box::into_raw(Box::new(CallbackProbe {
        node: CallbackNode::new(finish_probe),
        completed: completed.clone(),
        violations: violations.clone(),
        cpu,
        completion_waker,
    }));
    // SAFETY: raw field projection preserves the containing Box provenance.
    let node = NonNull::new(unsafe { &raw mut (*object).node }).unwrap();
    // SAFETY: uniquely owned stable unpublished Box, with transferable Arc
    // resources. Invocation and cancellation each consume the allocation once.
    unsafe { PreparedCallback::new(node, cancel_probe) }
}

fn submit_probes(
    cpu: usize,
    count: usize,
    completed: &Arc<AtomicUsize>,
    violations: &Arc<AtomicUsize>,
    completion_waker: Option<Waker>,
) -> usize {
    let completed = completed.clone();
    let callback_violations = violations.clone();
    let accepted = Arc::new(AtomicUsize::new(0));
    let submitted = accepted.clone();
    let producer = ktask::prepare_task(TaskInner::new_pidless_kthread(
        move || {
            let prepared: Vec<_> = (0..count)
                .map(|_| {
                    prepare_probe(
                        cpu,
                        &completed,
                        &callback_violations,
                        completion_waker.clone(),
                    )
                })
                .collect();
            // Accumulate the whole batch before this CPU can dispatch callbacks.
            // Preparation allocates above; submission holds no scheduler/RCU lock.
            let _guard = kspin::NoPreemptIrqSave::new();
            for prepared in prepared {
                // SAFETY: the fixture publishes no protected data; callback
                // ownership is unique and cleanup is nonblocking in softirq.
                match unsafe { prepared.submit() } {
                    Ok(()) => {
                        submitted.fetch_add(1, Ordering::Release);
                    }
                    Err((_, prepared)) => drop(prepared),
                }
            }
        },
        "rcu-callback-producer".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    producer.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu)));
    ktask::activate_task(&producer);
    let status = producer.join();
    // Join establishes producer completion, not callback completion.
    if status != 0 {
        violations.fetch_add(1, Ordering::Relaxed);
    }
    accepted.load(Ordering::Acquire)
}

#[def_test(serial)]
fn callback_admission_rejects_nmi_without_transferring_ownership() {
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let prepared = prepare_probe(0, &completed, &violations, None);
    let guard = kspin::NoPreemptIrqSave::new();
    let reject_fn = || {
        // SAFETY: unpublished owning fixture; NMI admission must reject before
        // transfer, leaving cancellation to the returned handle outside NMI.
        unsafe { prepared.submit() }
    };
    // SAFETY: strictly nested NMI under local exclusion. Admission rejects;
    // the unpublished owning handle is returned without invoking its callback.
    let result = unsafe { kirq::context::test_support::with_nmi_context(reject_fn) };
    drop(guard);
    let error = match result {
        Err((error, prepared)) => {
            drop(prepared);
            Some(error)
        }
        Ok(()) => None,
    };
    assert_eq!(error, Some(KError::InvalidInput));
    assert_eq!(completed.load(Ordering::Acquire), 0);
    assert_eq!(Arc::strong_count(&completed), 1);
}

#[def_test(serial)]
fn gp_task_consumes_repeated_callback_demand() {
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let mut accepted = 0;
    for _ in 0..8 {
        accepted += submit_probes(0, 1, &completed, &violations, Some(wait.waker()));
        while completed.load(Ordering::Acquire) < accepted {
            wait.park();
        }
    }
    assert_eq!(accepted, 8);
    assert_eq!(violations.load(Ordering::Acquire), 0);
}

#[def_test(serial)]
fn gp_task_delivers_scoped_callbacks_through_each_cpus_softirq() {
    // Boot installed the real action. A conflict must preserve it; the batches
    // below then exercise that original action with a fixed public workload.
    assert!(!RcuSoftirqIf::install(|| {}));
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
    let per_cpu = CALLBACKS_PER_CPU;
    let mut accepted = 0;
    for _ in 0..2 {
        for cpu in 0..kcpu_id_map::nr_cpus() {
            accepted += submit_probes(cpu, per_cpu, &completed, &violations, None);
        }
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
    assert_eq!(
        verify_deferred_reader_completion(LogicalCpuId::new(0)),
        unittest::TestResult::Ok
    );
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn migrated_deferred_reader_cleans_original_node_from_destination() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    assert_eq!(
        verify_deferred_reader_completion(LogicalCpuId::new(1)),
        unittest::TestResult::Ok
    );
}

fn verify_deferred_reader_completion(destination: LogicalCpuId) -> unittest::TestResult {
    let probe = Arc::new(DeferredReaderProbe::default());
    let reader_probe = probe.clone();
    let reader = ktask::prepare_task(TaskInner::new_pidless_kthread(
        move || {
            let task = ktask::current();
            let irq = krcu::with_read(|_| {
                reader_probe.has_entered.store(true, Ordering::Release);
                while !reader_probe.can_exit.load(Ordering::Acquire) {
                    if khal::percpu::this_cpu_id() == destination {
                        reader_probe
                            .has_reached_destination
                            .store(true, Ordering::Release);
                    }
                    core::hint::spin_loop();
                }
                // Return the exclusion guard, not the read token. The outer
                // scope exit must retain membership until an allowed boundary.
                kspin::IrqSave::new()
            });
            let cpu = khal::percpu::this_cpu_id();
            reader_probe.has_retained_cleanup.store(
                !task_reader(&task).is_reading()
                    && task_reader(&task).is_enrolled()
                    && cpu == destination
                    && krcu::reader_cleanup_requested_for_tests(destination),
                Ordering::Release,
            );
            drop(irq);
            let start_ns = khal::time::monotonic_time_nanos();
            while task_reader(&task).is_enrolled()
                && khal::time::monotonic_time_nanos().saturating_sub(start_ns) < 5_000_000_000
            {
                core::hint::spin_loop();
            }
            reader_probe
                .was_cleaned_at_boundary
                .store(!task_reader(&task).is_enrolled(), Ordering::Release);
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
    let was_enrolled = task_reader(&reader).is_enrolled();
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
    let retains_source = task_reader(&reader).is_enrolled_on(
        krcu_core::RcuContextIf::reader_node_for_tests(cpu.as_usize()),
    );
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let accepted = submit_probes(
        destination.as_usize(),
        1,
        &completed,
        &violations,
        Some(wait.waker()),
    ) == 1;
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
    assert!(!task_reader(&reader).is_enrolled());
    assert_eq!(completed.load(Ordering::Acquire), 1);
    assert_eq!(violations.load(Ordering::Relaxed), 0);
    unittest::TestResult::Ok
}

// ktask/ipi includes boot-time fixed notification setup, including self IPIs.
#[cfg(feature = "ipi")]
#[def_test(serial)]
fn reschedule_ipi_cleans_deferred_reader_after_cpu_observation() {
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
        unsafe { krcu_core::CallbackState::new(&NODES, core::array::from_fn(|id| id == cpu)) }
            .unwrap();
    let mut driver = callbacks.claim_driver().unwrap();
    let pin = kspin::NoPreempt::new();
    let marker = Box::into_raw(Box::new(CallbackNode::new(release_marker)));
    // SAFETY: this isolated marker protects no data. The driver owns it until
    // invoking its nonblocking release callback; rejection retains ownership.
    let accepted = unsafe { callbacks.submit(cpu, marker) };
    if !accepted {
        // SAFETY: rejected submission neither accesses nor retains the marker.
        drop(unsafe { Box::from_raw(marker) });
    }
    let started = driver.advance();
    krcu::with_read(|_| {
        // SAFETY: explicit outgoing-current fixture under IRQ/preemption
        // exclusion, static nodes; no real retired data uses this observation.
        unsafe { callbacks.note_context_switch(cpu, task_reader(&task)) };
    });
    let no_cpu_obligation = !callbacks.needs_cpu_observation(cpu);
    let retained = task_reader(&task).is_enrolled();
    // The private service decision is covered in krcu. This fixture exercises
    // the real IPI carrier with an already-observed, still-enrolled reader.
    let kicked = krcu::host::RcuNotifyIf::notify_reschedule(LogicalCpuId::new(cpu)).is_ok();
    let demand_retained = krcu::reader_cleanup_requested_for_tests(LogicalCpuId::new(cpu));
    // Suppress the pin's cleanup probe to leave the IRQ-only deferred case.
    ktask::with_preempt_check_deferred(|| drop(pin));
    drop(irq);
    let start_ns = khal::time::monotonic_time_nanos();
    while task_reader(&task).is_enrolled()
        && khal::time::monotonic_time_nanos().saturating_sub(start_ns) < 5_000_000_000
    {
        core::hint::spin_loop();
    }
    let cleaned_by_boundary = !task_reader(&task).is_enrolled();
    // Independently drain the fixture even if the tested delivery regresses.
    let _guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: retained current task at an excluded watched task boundary.
    let cleanup = unsafe { krcu_core::ReaderCleanup::new(task_reader(&task)) };
    krcu_core::RcuContextIf::finish_reader_cleanup(cleanup);
    // SAFETY: fresh ready check, no locks held, isolated fixture only.
    unsafe { callbacks.report_ready(cpu) };
    let is_active = driver.advance();
    // SAFETY: the controlled period is complete; the retained marker only
    // releases its allocation. No bookkeeping lock is held. Unlike the live
    // service probe, it needs no softirq context or interrupt read admission.
    let invoked = unsafe { callbacks.invoke_ready(cpu, 1) };
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(accepted && started && no_cpu_obligation && retained && kicked && demand_retained);
    assert!(cleaned_by_boundary);
    assert!(!is_active);
    assert_eq!(invoked, 1);
}
