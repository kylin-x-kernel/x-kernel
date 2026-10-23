// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host scope selection; enrollment is explicit, not timing-dependent preemption.

use krcu::{with_interrupt_read, with_read};
use krcu_core::{RcuContextIf, ReaderCleanup};
use unittest::{assert, assert_eq, def_test};

use super::rcu_support::reader;

fn enroll_current() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    // SAFETY: retained active current reader under IRQ/preemption exclusion;
    // the permanent host node outlives this scope and all cleanup boundaries.
    unsafe {
        RcuContextIf::reader_node_for_tests(khal::percpu::this_cpu_id().as_usize())
            .enroll(reader(&task))
    };
}

#[def_test]
fn nested_task_scopes_keep_membership_until_outer_error_return() {
    let task = ktask::current();
    let result = with_read(|_| {
        enroll_current();
        let inner_active = with_read(|_| reader(&task).is_reading());
        Err::<(), _>((inner_active, reader(&task).is_enrolled()))
    });
    assert_eq!(result, Err((true, true)));
    assert!(!reader(&task).is_reading());
    assert!(!reader(&task).is_enrolled());
}

#[def_test]
fn returned_preempt_guard_retains_membership_and_requests_cleanup() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = with_read(|_| {
        let guard = kspin::NoPreempt::new();
        enroll_current();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = !reader(&task).is_reading() && reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    // Observe the request without forcing a real switch in this local test.
    ktask::test_support::set_current_reschedule_pending(false);
    drop(guard);
    let cleaned = !reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn returned_bh_guard_defers_to_bh_enable() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = with_read(|_| {
        let guard = kirq::context::local_bh_disable();
        enroll_current();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(false);
    drop(guard);
    let cleaned = !reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn irq_only_exit_retains_request_until_a_safe_probe() {
    let task = ktask::current();
    let mut was_pending = false;
    let guard = with_read(|_| {
        let guard = kspin::IrqSave::new();
        enroll_current();
        was_pending = ktask::test_support::is_current_reschedule_pending();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(false);
    // Explicitly supply the boundary: do not claim IrqSave drop delivers an IPI.
    ktask::check_preempt_pending();
    let cleaned = !reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    drop(guard);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn nested_irq_and_nmi_scopes_preserve_the_outer_task_reader() {
    let task = ktask::current();
    let observed = with_read(|_| {
        enroll_current();
        let irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        let inside = with_interrupt_read(|_| {
            // SAFETY: synthetic IRQ already pins the initialized CPU/current
            // and masks IRQs. Nested NMI finishes its scope before returning.
            let nmi = unsafe {
                kirq::context::test_support::with_nmi_context(|| {
                    with_interrupt_read(|_| {
                        kirq::context::is_in_nmi()
                            && reader(&task).is_reading()
                            && reader(&task).is_enrolled()
                    })
                })
            };
            (nmi, reader(&task).is_reading(), reader(&task).is_enrolled())
        });
        drop(irq);
        (
            inside,
            reader(&task).is_reading(),
            reader(&task).is_enrolled(),
        )
    });
    assert_eq!(observed, ((true, true, true), true, true));
    assert!(!reader(&task).is_reading());
    assert!(!reader(&task).is_enrolled());
}

#[def_test]
fn nmi_exit_leaves_finished_reader_and_cleanup_demand_untouched() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    let cpu = khal::percpu::this_cpu_id();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    with_read(|_| enroll_current());
    let before = (
        reader(&task).is_reading(),
        reader(&task).is_enrolled(),
        krcu::reader_cleanup_requested_for_tests(cpu),
    );
    // SAFETY: initialized current and outer exclusion model interruption before
    // task cleanup; this NMI scope cannot schedule or alter enrollment.
    let active_in_nmi = unsafe {
        kirq::context::test_support::with_nmi_context(|| {
            with_interrupt_read(|_| reader(&task).is_reading())
        })
    };
    let after = (
        reader(&task).is_reading(),
        reader(&task).is_enrolled(),
        krcu::reader_cleanup_requested_for_tests(cpu),
    );
    // SAFETY: NMI has returned; current remains retained, watched and excluded.
    let cleanup = unsafe { ReaderCleanup::new(reader(&task)) };
    RcuContextIf::finish_reader_cleanup(cleanup);
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert_eq!(before, (false, true, true));
    assert_eq!(after, before);
    assert!(active_in_nmi);
    assert!(!reader(&task).is_enrolled());
    assert!(!krcu::reader_cleanup_requested_for_tests(cpu));
}

#[def_test]
fn quiescent_origin_nmi_scope_restores_watching_without_enrollment() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    let cpu = khal::percpu::this_cpu_id();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    // SAFETY: model the architecture's final user-entry callback with valid
    // current and no active reader. Only balanced NMI work and permanent atomic
    // observations occur in this pinned, IRQ-masked window, not user execution.
    unsafe { krcu::enter_user_quiescent() };
    let was_quiescent = !watching.is_watching();
    // SAFETY: the outer guard retains CPU/current and IRQ exclusion. The real
    // software NMI entry activates watching before this scoped reader.
    let active_in_nmi = unsafe {
        kirq::context::test_support::with_nmi_context(|| {
            with_interrupt_read(|_| watching.is_watching() && reader(&task).is_reading())
        })
    };
    let restored_quiescence = !watching.is_watching();
    // SAFETY: matching same-CPU callback after the nested NMI has fully exited.
    unsafe { krcu::exit_user_quiescent() };
    assert!(was_quiescent && active_in_nmi && restored_quiescence);
    assert!(watching.is_watching());
    assert!(!reader(&task).is_reading());
    assert!(!reader(&task).is_enrolled());
    assert!(!krcu::reader_cleanup_requested_for_tests(cpu));
}

#[def_test]
fn public_scope_enters_the_current_task_reader_and_preserves_error_return() {
    let task = ktask::current();
    let expected = reader(&task);
    let before = expected.is_reading();
    let mut calls = 0;
    let result = with_read(|_| {
        calls += 1;
        let during = expected.is_reading();
        let nested = with_read(|_| expected.is_reading());
        Err::<(), _>((during, nested, expected.is_reading()))
    });
    assert_eq!(calls, 1);
    assert!(!before);
    assert_eq!(result, Err((true, true, true)));
    assert!(!expected.is_reading());
}
