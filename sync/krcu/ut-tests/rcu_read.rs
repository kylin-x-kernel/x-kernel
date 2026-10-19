// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host scope selection; enrollment is explicit, not timing-dependent preemption.

use unittest::{assert, assert_eq, def_test};

use super::{with_interrupt_read, with_task_read};

fn enroll_current() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    // SAFETY: retained active current reader under IRQ/preemption exclusion;
    // the permanent host node outlives this scope and all cleanup boundaries.
    unsafe { crate::context::reader_node(khal::percpu::this_cpu_id()).enroll(crate::test_support::reader(&task)) };
}

#[def_test]
fn nested_task_scopes_keep_membership_until_outer_error_return() {
    let task = ktask::current();
    let result = with_task_read(|_| {
        enroll_current();
        let inner_active = with_task_read(|_| crate::test_support::reader(&task).is_reading());
        Err::<(), _>((inner_active, crate::test_support::reader(&task).is_enrolled()))
    });
    assert_eq!(result, Err((true, true)));
    assert!(!crate::test_support::reader(&task).is_reading());
    assert!(!crate::test_support::reader(&task).is_enrolled());
}

#[def_test]
fn returned_preempt_guard_retains_membership_and_requests_cleanup() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = with_task_read(|_| {
        let guard = kspin::NoPreempt::new();
        enroll_current();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = !crate::test_support::reader(&task).is_reading() && crate::test_support::reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    // Observe the request without forcing a real switch in this local test.
    ktask::test_support::set_current_reschedule_pending(false);
    drop(guard);
    let cleaned = !crate::test_support::reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn returned_bh_guard_defers_to_bh_enable() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = with_task_read(|_| {
        let guard = kirq::context::local_bh_disable();
        enroll_current();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = crate::test_support::reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(false);
    drop(guard);
    let cleaned = !crate::test_support::reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn irq_only_exit_retains_request_until_a_safe_probe() {
    let task = ktask::current();
    let mut was_pending = false;
    let guard = with_task_read(|_| {
        let guard = kspin::IrqSave::new();
        enroll_current();
        was_pending = ktask::test_support::is_current_reschedule_pending();
        ktask::test_support::set_current_reschedule_pending(false);
        guard
    });
    let retained = crate::test_support::reader(&task).is_enrolled();
    let requested = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(false);
    // Explicitly supply the boundary: do not claim IrqSave drop delivers an IPI.
    ktask::check_preempt_pending();
    let cleaned = !crate::test_support::reader(&task).is_enrolled();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    drop(guard);
    assert!(retained && requested && cleaned);
}

#[def_test]
fn cleanup_demand_survives_reentry_and_is_consumed_locally() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    let cpu = khal::percpu::this_cpu_id();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    with_task_read(|_| enroll_current());
    let requested = !crate::test_support::reader(&task).is_reading()
        && crate::test_support::reader(&task).is_enrolled()
        && crate::context::needs_reader_cleanup(cpu);
    let retained = with_task_read(|_| {
        // Reenter before the earlier deferred exit can be cleaned. The outer
        // exclusion prevents an entry-guard release from draining it first.
        // SAFETY: retained current task at an excluded task boundary; the
        // helper must leave active nesting and its original owner untouched.
        unsafe { crate::context::finish_reader_cleanup(crate::test_support::reader(&task)) };
        crate::context::needs_reader_cleanup(cpu) && crate::test_support::reader(&task).is_enrolled()
    });
    // SAFETY: the scope has returned; same current task and exclusion as above.
    unsafe { crate::context::finish_reader_cleanup(crate::test_support::reader(&task)) };
    let consumed = !crate::context::needs_reader_cleanup(cpu) && !crate::test_support::reader(&task).is_enrolled();
    // A notification after consumption must remain visible, even without an
    // owner. This is deterministic sequencing, not an injected NMI race.
    crate::context::request_local_cleanup_for_tests();
    let republished = crate::context::needs_reader_cleanup(cpu);
    // SAFETY: same safe boundary; stale demand must not require an enrollment.
    unsafe { crate::context::finish_reader_cleanup(crate::test_support::reader(&task)) };
    let stale_consumed = !crate::context::needs_reader_cleanup(cpu);
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(requested && retained && consumed && republished && stale_consumed);
}

#[def_test]
fn nested_irq_and_nmi_scopes_preserve_the_outer_task_reader() {
    let task = ktask::current();
    let observed = with_task_read(|_| {
        enroll_current();
        let irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        let inside = with_interrupt_read(|_| {
            // SAFETY: synthetic IRQ already pins the initialized CPU/current
            // and masks IRQs. Nested NMI finishes its scope before returning.
            let nmi = unsafe {
                kirq::context::test_support::with_nmi_context(|| {
                    with_interrupt_read(|_| {
                        kirq::context::is_in_nmi()
                            && crate::test_support::reader(&task).is_reading()
                            && crate::test_support::reader(&task).is_enrolled()
                    })
                })
            };
            (nmi, crate::test_support::reader(&task).is_reading(), crate::test_support::reader(&task).is_enrolled())
        });
        drop(irq);
        (inside, crate::test_support::reader(&task).is_reading(), crate::test_support::reader(&task).is_enrolled())
    });
    assert_eq!(observed, ((true, true, true), true, true));
    assert!(!crate::test_support::reader(&task).is_reading());
    assert!(!crate::test_support::reader(&task).is_enrolled());
}

#[def_test]
fn nmi_exit_leaves_finished_reader_and_cleanup_demand_untouched() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let task = ktask::current();
    let cpu = khal::percpu::this_cpu_id();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    with_task_read(|_| enroll_current());
    let before = (
        crate::test_support::reader(&task).is_reading(),
        crate::test_support::reader(&task).is_enrolled(),
        crate::context::needs_reader_cleanup(cpu),
    );
    // SAFETY: initialized current and outer exclusion model interruption before
    // task cleanup; this NMI scope cannot schedule or alter enrollment.
    let active_in_nmi = unsafe {
        kirq::context::test_support::with_nmi_context(|| {
            with_interrupt_read(|_| crate::test_support::reader(&task).is_reading())
        })
    };
    let after = (
        crate::test_support::reader(&task).is_reading(),
        crate::test_support::reader(&task).is_enrolled(),
        crate::context::needs_reader_cleanup(cpu),
    );
    // SAFETY: NMI has returned; current remains retained, watched and excluded.
    unsafe { crate::context::finish_reader_cleanup(crate::test_support::reader(&task)) };
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert_eq!(before, (false, true, true));
    assert_eq!(after, before);
    assert!(active_in_nmi);
    assert!(!crate::test_support::reader(&task).is_enrolled());
    assert!(!crate::context::needs_reader_cleanup(cpu));
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
    unsafe { crate::context::enter_user_quiescent() };
    let was_quiescent = !watching.is_watching();
    // SAFETY: the outer guard retains CPU/current and IRQ exclusion. The real
    // software NMI entry activates watching before this scoped reader.
    let active_in_nmi = unsafe {
        kirq::context::test_support::with_nmi_context(|| {
            with_interrupt_read(|_| watching.is_watching() && crate::test_support::reader(&task).is_reading())
        })
    };
    let restored_quiescence = !watching.is_watching();
    // SAFETY: matching same-CPU callback after the nested NMI has fully exited.
    unsafe { crate::context::exit_user_quiescent() };
    assert!(was_quiescent && active_in_nmi && restored_quiescence);
    assert!(watching.is_watching());
    assert!(!crate::test_support::reader(&task).is_reading());
    assert!(!crate::test_support::reader(&task).is_enrolled());
    assert!(!crate::context::needs_reader_cleanup(cpu));
}
