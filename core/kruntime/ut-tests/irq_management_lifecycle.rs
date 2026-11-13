// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real task registration/free serialization and CPU-local flow ownership.
//!
//! Local-flow tests invoke handlers with synthetic claims; no hardware source
//! is enabled. RetainedRunningDispatch tests substitute only admitted-operation accounting.
//! The permanent boot task/IRQ services supply the real mutex and drain waits.
//! Both UP and SMP are supported: all contenders may park and the coordinator
//! yields. An external QEMU timeout diagnoses a stalled handshake; elapsed time
//! is never a pass criterion. Test-owned actions are withdrawn before assertions.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::Completion;
use kirq::{IrqEvent, IrqHandler, tests_irq::RetainedRunningDispatch};
use ktask::TaskState;
use unittest::{assert, assert_eq, def_test};

#[cfg(target_arch = "x86_64")]
#[def_test(serial)]
fn msi_lookup_interleaves_with_release_and_reallocation() {
    struct Probe {
        request: AtomicUsize,
        acknowledged: AtomicUsize,
        hwirq: AtomicUsize,
        expected: AtomicUsize,
        stopped: AtomicBool,
        valid: AtomicBool,
    }
    let probe = Arc::new(Probe {
        request: AtomicUsize::new(0),
        acknowledged: AtomicUsize::new(0),
        hwirq: AtomicUsize::new(0),
        expected: AtomicUsize::new(usize::MAX),
        stopped: AtomicBool::new(false),
        valid: AtomicBool::new(true),
    });
    let reader = {
        let probe = probe.clone();
        ktask::spawn(move || {
            while !probe.stopped.load(Ordering::Acquire) {
                let request = probe.request.load(Ordering::Acquire);
                let observed =
                    kirq::translate_hwirq(kirq::MSI_DOMAIN, probe.hwirq.load(Ordering::Relaxed));
                if request != probe.acknowledged.load(Ordering::Relaxed) {
                    if observed.unwrap_or(usize::MAX) != probe.expected.load(Ordering::Relaxed) {
                        probe.valid.store(false, Ordering::Relaxed);
                    }
                    probe.acknowledged.store(request, Ordering::Release);
                }
                ktask::yield_now();
            }
        })
    };
    for round in 0..32 {
        let allocation = kirq::alloc_msix(kirq::IrqAffinity::Any).unwrap();
        let desc = kirq::descriptor(allocation.virq()).unwrap();
        probe.hwirq.store(desc.hwirq, Ordering::Relaxed);
        probe.expected.store(allocation.virq(), Ordering::Relaxed);
        probe.request.store(round * 2 + 1, Ordering::Release);
        while probe.acknowledged.load(Ordering::Acquire) != round * 2 + 1 {
            ktask::yield_now();
        }
        if !kirq::free_msix(allocation.virq()) {
            probe.valid.store(false, Ordering::Relaxed);
        }
        probe.expected.store(usize::MAX, Ordering::Relaxed);
        probe.request.store(round * 2 + 2, Ordering::Release);
        while probe.acknowledged.load(Ordering::Acquire) != round * 2 + 2 {
            ktask::yield_now();
        }
    }
    probe.stopped.store(true, Ordering::Release);
    let status = reader.join();
    assert_eq!(status, 0);
    assert!(probe.valid.load(Ordering::Acquire));
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn directory_and_domain_reuse_waits_for_old_borrowed_readers() {
    let result = check_index_reuse(false);
    if !result.is_ok() {
        return result;
    }
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn directory_reuse_preserves_interrupt_borrows_and_nested_nmi_queries() {
    let result = check_index_reuse(true);
    if !result.is_ok() {
        return result;
    }
}

#[cfg(feature = "smp")]
fn check_index_reuse(is_interrupt_reader: bool) -> unittest::TestResult {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
    use kirq::tests_irq::RcuIndexProbe;
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let old = Arc::new(RcuIndexProbe::new(0x5fb0, 0x5fb1));
    let entered = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let valid = Arc::new(AtomicBool::new(false));
    let reader = {
        let old = old.clone();
        let entered = entered.clone();
        let can_exit = can_exit.clone();
        let valid = valid.clone();
        ktask::prepare_task(ktask::TaskInner::new_pidless_kthread_with_entry(
            ktask::PreparedTaskEntry::new(move || {
                let wait_for_writer = || {
                    entered.store(true, Ordering::Release);
                    while !can_exit.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                };
                let matches = if is_interrupt_reader {
                    old.read_interrupt_across(wait_for_writer)
                } else {
                    old.read_across(wait_for_writer)
                };
                valid.store(matches, Ordering::Release);
            }).unwrap(),
            "irq-index-old-reader".into(),
            kbuild_config::TASK_STACK_SIZE,
        ))
    };
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    while !entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let retired = old.retire();
    let stayed_alive = !old.is_reclaimed();
    // Do not strand the reader if withdrawal unexpectedly rejects the fixture.
    let successor = retired.then(|| RcuIndexProbe::new(0x5fb0, 0x5fb1));
    let new_lookup = successor.as_ref().is_some_and(|new| new.read_across(|| {}));
    can_exit.store(true, Ordering::Release);
    let status = reader.join();
    drop(reader);
    let restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: the old reader has exited; callbacks need no resource we hold.
    let drained = unsafe { krcu::drain_callbacks() };
    let old_reclaimed = old.is_reclaimed();
    let successor_retired = successor.as_ref().is_some_and(RcuIndexProbe::retire);
    if !retired {
        old.retire();
    }
    // SAFETY: no fixture read scope or callback-owned lock remains held.
    let cleanup = unsafe { krcu::drain_callbacks() };
    assert!(retired && stayed_alive && new_lookup && restored);
    assert_eq!(status, 0);
    assert!(valid.load(Ordering::Acquire));
    assert!(drained.is_ok() && old_reclaimed);
    assert!(successor_retired && cleanup.is_ok());
    unittest::TestResult::Ok
}

#[cfg(feature = "ipi")]
#[def_test(serial)]
fn completed_cpu_calls_observe_body_and_capture_destruction() {
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    // Runtime UT starts after all boot CPU services have completed. This is a
    // boot-static participant set, not a present-CPU hotplug lease.
    let cpus = if cfg!(feature = "smp") {
        kcpu_id_map::nr_cpus()
    } else {
        1
    };
    for cpu in 0..cpus {
        let capture = Capture(drops.clone());
        let calls = calls.clone();
        assert_eq!(
            kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
                core::assert_eq!(khal::percpu::this_cpu_id().as_usize(), cpu);
                core::assert!(!karch::local_irq_enabled());
                calls.fetch_add(1, Ordering::Release);
                drop(capture);
            }),
            Ok(())
        );
        assert_eq!(drops.load(Ordering::Acquire), cpu + 1);
    }
    assert_eq!(calls.load(Ordering::Acquire), cpus);
}

#[cfg(feature = "ipi")]
#[def_test(serial)]
fn nmi_shutdown_retains_original_owner_across_remote_failure_and_retry() {
    use kcpu_id_map::LogicalCpuId;
    use kirq::tests_irq::NmiShutdownProbe;

    if !cfg!(feature = "smp") || kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(NmiShutdownProbe::new(
        0x5fba,
        Arc::new(HandlerProbe {
            drops: drops.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    ));
    // This source owner excludes restart across both passes. Runtime UT runs
    // after boot-static CPU services; no hotplug or system stop is permitted.
    let owner = kmutex::Mutex::new(());
    let _owner = owner.lock();
    let completed = Arc::new(AtomicBool::new(false));
    for cpu in 0..2 {
        let source = source.clone();
        let result = completed.clone();
        kipi::run_on_cpu_sync(LogicalCpuId::new(cpu), move || {
            result.store(source.prepare_local(), Ordering::Release);
        })
        .unwrap();
        assert!(completed.load(Ordering::Acquire));
    }
    let shutdown = Arc::new(source.begin());
    for cpu in 0..kbuild_config::NR_CPUS {
        assert_eq!(shutdown.is_participant(cpu), cpu < 2);
    }
    for cpu in 0..2 {
        let local_source = source.clone();
        let retained = shutdown.clone();
        let result = completed.clone();
        kipi::run_on_cpu_sync(LogicalCpuId::new(cpu), move || {
            core::assert!(retained.is_participant(cpu));
            // CPU1 stops but does not yet acknowledge controller teardown.
            result.store(local_source.stop_local(cpu == 0), Ordering::Release);
            drop(retained);
        })
        .unwrap();
        assert_eq!(completed.load(Ordering::Acquire), cpu == 0);
        assert!(!source.remove_without_claim());
        assert_eq!(drops.load(Ordering::Acquire), 0);
    }
    // Successful transport return includes capture destruction. Only then can
    // the owner recover the unique claim; incomplete local teardown still fails.
    let shutdown = Arc::try_unwrap(shutdown).ok().unwrap();
    let shutdown = source.finish(shutdown).err().unwrap();
    assert_eq!(drops.load(Ordering::Acquire), 0);
    let shutdown = Arc::new(shutdown);
    let retained = shutdown.clone();
    let retry_source = source.clone();
    let result = completed.clone();
    kipi::run_on_cpu_sync(LogicalCpuId::new(1), move || {
        core::assert!(retained.is_participant(1));
        result.store(retry_source.stop_local(true), Ordering::Release);
        drop(retained);
    })
    .unwrap();
    assert!(completed.load(Ordering::Acquire));
    let shutdown = Arc::try_unwrap(shutdown).ok().unwrap();
    assert!(source.finish(shutdown).is_ok());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    // Fake controller acknowledgements prove lifecycle composition, not GIC
    // delivery or that the surrounding IPI claim has completed.
}

#[cfg(feature = "ipi")]
#[def_test(serial)]
fn local_action_retirement_requires_owner_shutdown_and_restart_exclusion() {
    let virq = 0x5f83;
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let probe = Arc::new(kirq::tests_irq::LocalFlowProbe::new(
        virq,
        Arc::new(HandlerProbe {
            drops: drops.clone(),
            calls: calls.clone(),
        }),
    ));
    let cpus = if cfg!(feature = "smp") {
        kcpu_id_map::nr_cpus()
    } else {
        1
    };
    for cpu in 0..cpus {
        let probe = probe.clone();
        kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
            let _pin = kspin::NoPreemptIrqSave::new();
            probe.set_enabled(true);
            // This synchronous synthetic dispatch includes its claim completion.
            // No source callback remains admitted when the owner starts closing.
            core::assert!(probe.dispatch());
        })
        .unwrap();
    }
    assert_eq!(calls.load(Ordering::Acquire), cpus);
    assert_eq!(drops.load(Ordering::Acquire), 0);

    let is_stopping = Arc::new(kmutex::Mutex::new(false));
    let stopped = Arc::new(Completion::new());
    let allow_retirement = Arc::new(Completion::new());
    let did_free = Arc::new(AtomicBool::new(false));
    let free_task = {
        let owner = is_stopping.clone();
        let stopped = stopped.clone();
        let allow_retirement = allow_retirement.clone();
        let probe = probe.clone();
        let did_free = did_free.clone();
        ktask::spawn(move || {
            let mut stopping = owner.lock();
            *stopping = true;
            for cpu in 0..cpus {
                let local = probe.clone();
                kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
                    local.set_enabled(false);
                })
                .unwrap();
            }
            stopped.complete_all();
            // Test the source-owner gate independently of KIRQ's ordinary drain.
            kiface_task_wait_api::wait_for_completion(&allow_retirement).unwrap();
            // SAFETY: this software source has no hardware producer. Every
            // dispatch and its synthetic completion returned before this task
            // started; all local slots are now stopped. The owner mutex and
            // stopping flag exclude every restart until retirement finishes.
            let removed = unsafe { kirq::try_free_percpu_irq(virq) };
            did_free.store(matches!(&removed, Ok(Some(_))), Ordering::Release);
            drop(stopping);
            drop(removed);
        })
    };
    kiface_task_wait_api::wait_for_completion(&stopped).unwrap();
    let drops_before_retirement = drops.load(Ordering::Acquire);
    let free_before_permission = did_free.load(Ordering::Acquire);
    let did_reenable = Arc::new(AtomicBool::new(false));
    let reenable_started = Arc::new(Completion::new());
    let reenable_task = {
        let owner = is_stopping.clone();
        let did_reenable = did_reenable.clone();
        let started = reenable_started.clone();
        ktask::spawn(move || {
            started.complete_all();
            let stopping = owner.lock();
            did_reenable.store(!*stopping, Ordering::Release);
        })
    };
    kiface_task_wait_api::wait_for_completion(&reenable_started).unwrap();
    while !matches!(
        reenable_task.state(),
        TaskState::Blocked | TaskState::Exited
    ) {
        ktask::yield_now();
    }
    let is_enable_excluded = reenable_task.state() == TaskState::Blocked;
    allow_retirement.complete_all();
    let statuses = [free_task.join(), reenable_task.join()];
    drop(probe);
    assert_eq!(statuses, [0, 0]);
    assert!(is_enable_excluded);
    assert!(!free_before_permission);
    assert_eq!(drops_before_retirement, 0);
    assert!(did_free.load(Ordering::Acquire));
    assert!(!did_reenable.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(kirq::descriptor(virq).is_none());
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn local_flow_handlers_overlap_on_distinct_cpus() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let entered = Arc::new(AtomicUsize::new(0));
    let calls = entered.clone();
    let probe = Arc::new(kirq::tests_irq::LocalFlowProbe::new(
        0x5f82,
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::AcqRel);
            // Both pinned CPUs must enter before either callback can finish.
            // External QEMU timeout detects an accidental global running gate.
            while calls.load(Ordering::Acquire) < 2 {
                core::hint::spin_loop();
            }
            IrqEvent::HANDLED
        }),
    ));
    let mut tasks = alloc::vec::Vec::new();
    for cpu in 0..2 {
        let probe = probe.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread_with_entry(
            ktask::PreparedTaskEntry::new(move || {
                let pin = kspin::NoPreemptIrqSave::new();
                probe.set_enabled(true);
                let completed = probe.dispatch();
                probe.set_enabled(false);
                // Disabled dispatch still completes but must not invoke the action.
                let disabled_completed = probe.dispatch();
                drop(pin);
                drop(probe);
                ktask::exit(if completed && disabled_completed {
                    0
                } else {
                    1
                });
            }).unwrap(),
            "local_irq_overlap".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu)));
        tasks.push(task);
    }
    {
        // The first IRQ-off handler must not preempt this coordinator before
        // its peer is runnable. Prepare allocations and affinity above, then
        // publish both tasks before allowing a local scheduling checkpoint.
        let _pin = kspin::NoPreemptIrqSave::new();
        for task in &tasks {
            ktask::activate_task(task);
        }
    }
    let statuses: alloc::vec::Vec<_> = tasks.into_iter().map(|task| task.join()).collect();
    assert!(statuses.iter().all(|status| *status == 0));
    assert_eq!(entered.load(Ordering::Acquire), 2);
    drop(probe);
    assert!(kirq::descriptor(0x5f82).is_none());
}

struct HandlerProbe {
    drops: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

impl IrqHandler for HandlerProbe {
    fn handle(&self, _irq: usize) -> IrqEvent {
        self.calls.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }
}

impl Drop for HandlerProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

#[def_test(serial)]
fn masked_registration_rejects_normal_admission_but_allows_explicit_startup() {
    let virq = 0x5f72;
    assert!(kirq::descriptor(virq).is_none());
    let handler: Arc<dyn IrqHandler> = Arc::new(|_| IrqEvent::HANDLED);
    let (ordinary, was_absent, startup, stayed_masked, _binding) = {
        let _irq = kspin::IrqSave::new();
        let ordinary = kirq::try_register_disabled(virq, handler.clone());
        let was_absent = kirq::descriptor(virq).is_none();
        let binding = kirq::tests_irq::SyntheticLine::new(kirq::IrqDesc::from_virq(virq));
        assert_eq!(kirq::try_map(kirq::IrqDesc::from_virq(virq)), Ok(virq));
        let startup =
            kirq::try_register_startup(virq, handler.clone(), kirq::IrqRegisterOptions::disabled());
        (
            ordinary,
            was_absent,
            startup,
            !karch::local_irq_enabled(),
            binding,
        )
    };
    let removed = kirq::try_free_irq(virq);
    let did_remove = matches!(&removed, Ok(Some(_)));
    drop(removed);
    assert!(matches!(
        ordinary,
        Err(kirq::IrqDescError::InvalidContext { .. })
    ));
    assert!(was_absent);
    assert_eq!(startup, Ok(true));
    assert!(stayed_masked);
    assert!(did_remove);
    assert!(kirq::descriptor(virq).is_some());
    assert!(kirq::tests_irq::retire_unused_test_line(virq));
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn read_scope_rejects_normal_registration_before_publication() {
    let virq = 0x5f73;
    assert!(kirq::descriptor(virq).is_none());
    let handler: Arc<dyn IrqHandler> = Arc::new(|_| IrqEvent::HANDLED);
    let (rejected, was_absent) = krcu::with_read(|_| {
        let result = kirq::try_register_disabled(virq, handler.clone());
        (result, kirq::descriptor(virq).is_none())
    });
    let map_rejected = krcu::with_read(|_| kirq::try_map(kirq::IrqDesc::from_virq(virq)));
    assert!(matches!(
        map_rejected,
        Err(kirq::IrqDescError::InvalidContext { .. })
    ));
    assert!(kirq::descriptor(virq).is_none());
    // This is an ordinary-admission check only. It deliberately makes no claim
    // about startup registration being valid while retaining an RCU read scope.
    let _binding = kirq::tests_irq::SyntheticLine::new(kirq::IrqDesc::from_virq(virq));
    assert_eq!(kirq::try_map(kirq::IrqDesc::from_virq(virq)), Ok(virq));
    let after_scope = kirq::try_register_disabled(virq, handler.clone());
    let removed = kirq::try_free_irq(virq);
    let did_remove = matches!(&removed, Ok(Some(_)));
    drop(removed);
    assert!(matches!(
        rejected,
        Err(kirq::IrqDescError::InvalidContext { .. })
    ));
    assert!(was_absent);
    assert_eq!(after_scope, Ok(true));
    assert!(did_remove);
    assert!(kirq::descriptor(virq).is_some());
    assert!(kirq::tests_irq::retire_unused_test_line(virq));
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn request_waits_for_free_to_drain_retained_inflight() {
    // This synthetic number is never enabled or passed to a controller. Reject
    // a collision rather than modifying a pre-existing registration.
    let virq = 0x5f70;
    let other_virq = 0x5f71;
    assert!(kirq::descriptor(virq).is_none());
    assert!(kirq::descriptor(other_virq).is_none());
    let _binding = kirq::tests_irq::SyntheticLine::new(kirq::IrqDesc::from_virq(virq));
    assert_eq!(kirq::try_map(kirq::IrqDesc::from_virq(virq)), Ok(virq));
    let _other_binding = kirq::tests_irq::SyntheticLine::new(kirq::IrqDesc::from_virq(other_virq));
    assert_eq!(
        kirq::try_map(kirq::IrqDesc::from_virq(other_virq)),
        Ok(other_virq)
    );
    let old_drops = Arc::new(AtomicUsize::new(0));
    let new_drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        kirq::try_register_disabled(
            virq,
            Arc::new(HandlerProbe {
                drops: old_drops.clone(),
                calls: calls.clone(),
            })
        )
        .unwrap()
    );
    let running = RetainedRunningDispatch::begin(virq).unwrap();
    let free_started = Arc::new(Completion::new());
    let free_succeeded = Arc::new(AtomicBool::new(false));
    let free_task = {
        let started = free_started.clone();
        let succeeded = free_succeeded.clone();
        ktask::spawn(move || {
            started.complete_all();
            let removed = kirq::try_free_irq(virq);
            let did_remove = matches!(&removed, Ok(Some(_)));
            drop(removed);
            succeeded.store(did_remove, Ordering::Release);
        })
    };
    kiface_task_wait_api::wait_for_completion(&free_started).unwrap();
    while !running.is_teardown_in_progress() && free_task.state() != TaskState::Exited {
        ktask::yield_now();
    }
    while !matches!(free_task.state(), TaskState::Blocked | TaskState::Exited) {
        ktask::yield_now();
    }
    let has_teardown_waiter =
        running.is_teardown_in_progress() && free_task.state() == TaskState::Blocked;

    let request_started = Arc::new(Completion::new());
    let request_succeeded = Arc::new(AtomicBool::new(false));
    let request_task = {
        let started = request_started.clone();
        let succeeded = request_succeeded.clone();
        let handler = Arc::new(HandlerProbe {
            drops: new_drops.clone(),
            calls: calls.clone(),
        });
        ktask::spawn(move || {
            started.complete_all();
            succeeded.store(
                kirq::try_register_disabled(virq, handler) == Ok(true),
                Ordering::Release,
            );
        })
    };
    kiface_task_wait_api::wait_for_completion(&request_started).unwrap();
    while !matches!(request_task.state(), TaskState::Blocked | TaskState::Exited) {
        ktask::yield_now();
    }
    // The start completions exclude task-startup blocking. Neither worker has
    // another blocking operation before the IRQ API. State observations confirm
    // actual waits, not merely an intent flag published before lock acquisition.
    let has_request_waiter = request_task.state() == TaskState::Blocked;
    let has_live_descriptor = kirq::descriptor(virq).is_some();
    let old_drops_before_release = old_drops.load(Ordering::Acquire);
    let new_drops_before_release = new_drops.load(Ordering::Acquire);
    let did_free_early = free_succeeded.load(Ordering::Acquire);
    let did_register_early = request_succeeded.load(Ordering::Acquire);

    // A sleeping transaction excludes its own Line, not unrelated management.
    let other_registered = kirq::try_register_disabled(other_virq, Arc::new(|_| IrqEvent::HANDLED));
    // Already disabled: exercise control and drain admission without issuing a
    // controller write for this synthetic IRQ number.
    let other_disabled = kirq::try_disable_irq_nosync(other_virq);
    let other_synchronized = kirq::try_synchronize_irq(other_virq);
    let other_removed = kirq::try_free_irq(other_virq);
    let did_remove_other = matches!(&other_removed, Ok(Some(_)));
    drop(other_removed);

    drop(running);
    let free_status = free_task.join();
    let request_status = request_task.join();
    let final_removed = kirq::try_free_irq(virq);
    let did_remove_replacement = matches!(&final_removed, Ok(Some(_)));
    drop(final_removed);

    assert_eq!([free_status, request_status], [0, 0]);
    assert!(has_teardown_waiter);
    assert!(has_request_waiter);
    assert!(has_live_descriptor);
    assert!(!did_free_early);
    assert!(!did_register_early);
    assert_eq!(other_registered, Ok(true));
    assert_eq!(other_disabled, Ok(()));
    assert_eq!(other_synchronized, Ok(true));
    assert!(did_remove_other);
    assert!(kirq::descriptor(other_virq).is_some());
    assert!(kirq::tests_irq::retire_unused_test_line(other_virq));
    assert!(kirq::descriptor(other_virq).is_none());
    assert_eq!([old_drops_before_release, new_drops_before_release], [0, 0]);
    assert!(free_succeeded.load(Ordering::Acquire));
    assert!(request_succeeded.load(Ordering::Acquire));
    assert!(did_remove_replacement);
    assert!(kirq::descriptor(virq).is_some());
    assert!(kirq::tests_irq::retire_unused_test_line(virq));
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(old_drops.load(Ordering::Acquire), 1);
    assert_eq!(new_drops.load(Ordering::Acquire), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}
