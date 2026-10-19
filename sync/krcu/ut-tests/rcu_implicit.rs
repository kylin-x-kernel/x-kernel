// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real guards and watching with an isolated GP, not the boot callback carrier.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu_core::{CallbackNode, CallbackState, ReaderNode};
use unittest::{assert, assert_eq, def_test};

// Separate CPUs let the updater observe without forcing the reader to schedule.
const READER_CPU: usize = 1;
const UPDATER_CPU: usize = 0;

#[repr(C)]
struct Value {
    node: CallbackNode,
    number: usize,
    drops: Arc<AtomicUsize>,
}

const _: () = core::assert!(core::mem::offset_of!(Value, node) == 0);

impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

/// # Safety
/// `node` is the zero-offset field projection of a uniquely submitted Value
/// Box. The sole executor invokes it once, after the reader has ended its borrow.
unsafe fn reclaim_value(node: *mut CallbackNode) {
    // SAFETY: checked zero offset preserves the original Box pointer and its
    // provenance; invocation consumes the transferred owner after all accesses.
    drop(unsafe { Box::from_raw(node.cast::<Value>()) });
}

#[derive(Default)]
struct Handshake {
    has_entered: AtomicBool,
    can_exit: AtomicBool,
    has_exited: AtomicBool,
    can_resume: AtomicBool,
    has_valid_read: AtomicBool,
    has_timed_out: AtomicBool,
}

struct Observation {
    reported: bool,
    is_cpu_pending: bool,
    is_active: bool,
    has_ready: bool,
}

fn verify_implicit_region<G>(enter_fn: impl FnOnce() -> G + Send + 'static) -> unittest::TestResult {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let published = Arc::new(AtomicPtr::new(Box::into_raw(Box::new(Value {
        node: CallbackNode::new(reclaim_value),
        number: 17,
        drops: drops.clone(),
    }))));
    let handshake = Arc::new(Handshake::default());
    let result = Arc::new(kspin::SpinNoPreempt::new(None));
    let updater_published = published.clone();
    let updater_handshake = handshake.clone();
    let updater_result = result.clone();
    let updater = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let area = kcpu_slot::area(LogicalCpuId::new(READER_CPU)).unwrap();
            let watching = super::cpu_watching(&area);
            let nodes = [const { ReaderNode::new() }; kbuild_config::NR_CPUS];
            // SAFETY: sole controller of these isolated nodes. Only READER_CPU reads
            // this fixture's publication; no production reader uses the nodes.
            let state = unsafe { CallbackState::new(&nodes, core::array::from_fn(|cpu| cpu == READER_CPU)) }
                .unwrap();
            let mut driver = state.claim_driver().unwrap();
            while !updater_handshake.has_entered.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            let is_updater_on_expected_cpu = khal::percpu::this_cpu_id() == LogicalCpuId::new(UPDATER_CPU);
            let retired = updater_published.swap(core::ptr::null_mut(), Ordering::AcqRel);
            // SAFETY: the original allocation is retained, and only the value
            // field is borrowed. Projecting the disjoint node preserves provenance.
            let node = unsafe { &raw mut (*retired).node };
            // SAFETY: withdrawal precedes submission; the sole reader's guard
            // and permanent watching state cover all accesses. This executor
            // also waits for borrow exit before invocation, even on regression.
            let accepted = unsafe { state.submit(READER_CPU, node) };
            let started = driver.advance();
            let observed = core::array::from_fn::<_, 2, _>(|_| {
                // SAFETY: actual permanent reader CPU state, sampled from watched
                // task context without locks. No synthetic QS is supplied.
                let reported = unsafe { state.observe_watching(READER_CPU, watching) };
                Observation {
                    reported,
                    is_cpu_pending: state.needs_cpu_observation(READER_CPU),
                    is_active: driver.advance(),
                    has_ready: state.has_ready(READER_CPU),
                }
            });
            updater_handshake.can_exit.store(true, Ordering::Release);
            while !updater_handshake.has_exited.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            // Observe the controlled user boundary after borrow/guard exit.
            // The bounded QEMU runner diagnoses stalled progress without
            // fabricating a coordinator report to clean up.
            while accepted && !state.has_ready(READER_CPU) {
                // SAFETY: fresh observation of the same permanent reader CPU state;
                // all fixture borrows have ended, no bookkeeping lock is held.
                unsafe { state.observe_watching(READER_CPU, watching) };
                driver.advance();
                ktask::yield_now();
            }
            // SAFETY: real observations made the callback eligible, and the
            // explicit exit handshake independently prevents a regression UAF.
            let invoked = unsafe { state.invoke_ready(READER_CPU, 1) };
            if !accepted {
                // SAFETY: rejection retained ownership; reader exit is confirmed.
                drop(unsafe { Box::from_raw(retired) });
            }
            updater_handshake.can_resume.store(true, Ordering::Release);
            *updater_result.lock() = Some((is_updater_on_expected_cpu, accepted, started, observed, invoked));
        },
        "rcu-implicit-updater".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    updater.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(UPDATER_CPU)));

    let reader_handshake = handshake.clone();
    let reader_published = published.clone();
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let guard = enter_fn();
            let pointer = reader_published.load(Ordering::Acquire);
            // SAFETY: publication remains installed until has_entered; after
            // withdrawal the updater never invokes reclamation before has_exited.
            let old = unsafe { &(*pointer).number };
            let task = ktask::current();
            let has_no_explicit_reader = !crate::test_support::reader(&task).is_reading() && !crate::test_support::reader(&task).is_enrolled();
            reader_handshake.has_entered.store(true, Ordering::Release);
            // An IRQ-off reader cannot depend on a timer callback to escape a
            // broken fixture. The hardware clock still advances with IRQs masked.
            let deadline = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
            while !reader_handshake.can_exit.load(Ordering::Acquire) {
                if khal::time::monotonic_time_nanos() >= deadline {
                    reader_handshake.has_timed_out.store(true, Ordering::Release);
                    break;
                }
                core::hint::spin_loop();
            }
            reader_handshake.has_valid_read.store(
                *old == 17 && has_no_explicit_reader
                    && khal::percpu::this_cpu_id() == LogicalCpuId::new(READER_CPU),
                Ordering::Release,
            );
            drop(guard);
            let boundary = kspin::NoPreemptIrqSave::new();
            // The object borrow and tested guard have ended, not the task.
            reader_handshake.has_exited.store(true, Ordering::Release);
            // SAFETY: retained scope-free current, IRQ/preemption excluded and
            // single-CPU affinity. This controlled boundary contains only
            // independent handshake atomics, no scheduling or RCU accesses.
            unsafe { super::enter_user_quiescent() };
            while !reader_handshake.can_resume.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            // SAFETY: matching same-CPU exit before any task work or guard release.
            unsafe { super::exit_user_quiescent() };
            drop(boundary);
        },
        "rcu-implicit-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(READER_CPU)));
    // Publish the releaser first: the reader must never monopolize a CPU while
    // waiting for a task the test has not activated yet. The releaser can yield.
    ktask::activate_task(&updater);
    ktask::activate_task(&reader);
    let reader_status = reader.join();
    let updater_status = updater.join();
    let (is_updater_on_expected_cpu, accepted, started, observed, invoked) = result.lock().take().unwrap();
    assert_eq!((reader_status, updater_status), (0, 0));
    assert!(!handshake.has_timed_out.load(Ordering::Acquire));
    assert!(handshake.has_valid_read.load(Ordering::Acquire));
    assert!(is_updater_on_expected_cpu && accepted && started);
    for sample in observed {
        assert!(!sample.reported && sample.is_cpu_pending && sample.is_active && !sample.has_ready);
    }
    assert!(published.load(Ordering::Acquire).is_null());
    assert_eq!(invoked, 1);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn irq_off_region_delays_object_retirement() -> unittest::TestResult {
    verify_implicit_region(kspin::IrqSave::new)
}

#[def_test(serial)]
fn preempt_off_region_delays_object_retirement() -> unittest::TestResult {
    verify_implicit_region(kspin::NoPreempt::new)
}

#[def_test(serial)]
fn bh_off_region_delays_object_retirement() -> unittest::TestResult {
    verify_implicit_region(kirq::context::local_bh_disable)
}
