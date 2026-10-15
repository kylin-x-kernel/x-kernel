// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-owned RCU GP task integration. The task and its state intentionally persist
//! for the kernel lifetime; these tests neither replace nor reinitialize them.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::AtomicUsize;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu::CallbackNode;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn action_conflict_preserves_control_for_startup_retry() {
    // These isolated nodes have static storage but no GP is started and no
    // reader is enrolled. The fixture never touches the live boot service.
    static NODES: [krcu::ReaderNode; kbuild_config::NR_CPUS] =
        [const { krcu::ReaderNode::new() }; kbuild_config::NR_CPUS];
    // SAFETY: this fixture is the sole coordinator of these inactive nodes.
    let callbacks = unsafe { CallbackState::new(&NODES, core::array::from_fn(|cpu| cpu == 0)) }
        .unwrap();
    let service = GracePeriodTask {
        callbacks,
        has_started: kspin::SpinNoPreempt::new(false),
        waker: Once::new(),
        has_demand: AtomicBool::new(false),
    };
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
fn gp_task_consumes_repeated_retained_demand() {
    let service = SERVICE.get().unwrap();
    // No ordinary RCU producers are admitted yet. Each request is a genuine
    // pass through the boot-created task, not a test-side clear of its flag.
    for _ in 0..8 {
        service.request_pass();
        while service.has_demand.load(Ordering::Acquire) {
            crate::yield_now();
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
        || crate::is_sleepable_context()
        || khal::percpu::this_cpu_id().as_usize() != probe.cpu
    {
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
fn gp_task_delivers_callbacks_through_each_cpus_softirq() {
    let service = SERVICE.get().unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let is_finished = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    // This fixture supplies real scheduler boundaries on every participant.
    // It does not simulate observations or claim idle/no-tick progress coverage.
    for cpu in 0..kcpu_id_map::nr_cpus() {
        let finished = is_finished.clone();
        let worker = crate::prepare_task(TaskInner::new_pidless_kthread(
            move || {
                while !finished.load(Ordering::Acquire) {
                    crate::yield_now();
                }
            },
            "rcu-observer-test".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        worker.set_cpumask(crate::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu)));
        crate::activate_task(&worker);
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
            crate::yield_now();
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
