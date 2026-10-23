// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Safe-boundary cleanup without a requested context switch.

use krcu_core::{RcuContextIf, ReaderCleanup};
use unittest::{assert, assert_eq, def_test};

use super::rcu_support::reader;

/// # Safety
/// `task` is the retained current task; IRQs/preemption are excluded. The
/// caller must reach a cleanup boundary before releasing its fixture.
unsafe fn leave_deferred_reader(task: &ktask::TaskInner) {
    let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
        let node = RcuContextIf::reader_node_for_tests(khal::percpu::this_cpu_id().as_usize());
        // SAFETY: isolated current-task fixture under scheduler exclusion;
        // the static node and retained task outlive the caller's cleanup.
        unsafe { node.enroll(reader(task)) };
    };
    // SAFETY: current-task fixture under exclusion, no parallel scope access;
    // the caller retains storage and supplies the later safe cleanup boundary.
    unsafe { reader(task).with_read_deferred(enroll_fn) };
}

#[def_test]
fn host_preemption_controls_preserve_deferred_cleanup_until_checkpoint() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(false);
    let mut calls = 0;
    let mut observations = None;
    krcu::host::RcuTaskIf::with_preempt_check_deferred(&mut || {
        calls += 1;
        // SAFETY: host's body supplies preemption exclusion; the outer IRQ
        // guard pins current. The explicit checkpoint below drains enrollment.
        unsafe { leave_deferred_reader(&task) };
        krcu::host::RcuTaskIf::request_current_reschedule();
        observations = Some((
            krcu::host::RcuTaskIf::current_preemption_disabled(),
            !karch::local_irq_enabled(),
        ));
    });
    let (was_excluded, irqs_stayed_masked) = observations.unwrap();
    let remained_enrolled = reader(&task).is_enrolled();
    let remained_pending = ktask::test_support::is_current_reschedule_pending();
    let credit_released = !krcu::host::RcuTaskIf::current_preemption_disabled();
    krcu::host::RcuTaskIf::check_preempt_pending();
    let cleaned = !reader(&task).is_enrolled();

    // Restore the fixture independently before an assertion can return early.
    ktask::with_preempt_check_deferred(|| {
        // SAFETY: retained current, IRQ/preemption excluded and zero nesting;
        // static owner remains live, with no node lock held.
        let cleanup = unsafe { ReaderCleanup::new(reader(&task)) };
        RcuContextIf::finish_reader_cleanup(cleanup);
    });
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert_eq!(calls, 1);
    assert!(was_excluded);
    assert!(irqs_stayed_masked);
    assert!(remained_enrolled);
    assert!(remained_pending);
    assert!(credit_released);
    assert!(cleaned);
    assert!(!karch::local_irq_enabled());
}

#[def_test]
fn final_preempt_enable_cleans_without_reschedule_request() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = kspin::NoPreempt::new();
    // SAFETY: current task retained, IRQ/preemption excluded until guard drop.
    unsafe { leave_deferred_reader(&task) };
    ktask::test_support::set_current_reschedule_pending(false);
    ktask::check_preempt_pending();
    let retained_while_excluded = reader(&task).is_enrolled();
    drop(guard);
    let cleaned = !reader(&task).is_enrolled();
    let has_requested_switch = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(was_pending);
    assert!(retained_while_excluded);
    assert!(cleaned);
    assert!(!has_requested_switch);
    assert!(!ktask::current_preemption_disabled());
    assert!(!karch::local_irq_enabled());
}

#[def_test]
fn active_reentry_survives_preempt_check() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let guard = kspin::NoPreempt::new();
    // SAFETY: retained current reader; the normal outer exit below cleans it.
    unsafe { leave_deferred_reader(&task) };
    // Keep the core scope's immediate cleanup on exit: public with_read would
    // deliberately defer that exit while the outer IRQ guard is still held.
    let observed = reader(&task).with_read(|_| {
        drop(guard);
        ktask::check_preempt_pending();
        (reader(&task).is_reading(), reader(&task).is_enrolled())
    });
    assert_eq!(observed, (true, true));
    assert!(!reader(&task).is_enrolled());
}

#[def_test]
fn bottom_half_exclusion_retains_cleanup_until_enable() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let guard = kirq::context::local_bh_disable();
    // SAFETY: BH guard excludes preemption and the outer guard masks IRQs;
    // dropping the BH guard below supplies the safe cleanup boundary.
    unsafe { leave_deferred_reader(&task) };
    ktask::check_preempt_pending();
    let retained = reader(&task).is_enrolled();
    drop(guard);
    assert!(retained);
    assert!(!reader(&task).is_enrolled());
    assert!(!ktask::current_preemption_disabled());
}

/// # Safety
/// The marker must remain live and uniquely queued until this detached callback
/// returns. Its stack owner retains storage; this callback neither frees nor
/// accesses it. No protected object is retired by this fixture.
unsafe fn complete_marker(_node: *mut krcu_core::CallbackNode) {}

#[def_test]
fn cleanup_after_cpu_report_releases_callback_without_another_observation() {
    let _irq = kspin::IrqSave::new();
    let task = ktask::current();
    let was_pending = ktask::test_support::is_current_reschedule_pending();
    let guard = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let nodes = [const { krcu_core::ReaderNode::new() }; kbuild_config::NR_CPUS];
    // SAFETY: isolated nodes, controlled exclusively by this fixture. No real
    // retired object or production GP uses these simulated observations.
    let state =
        unsafe { krcu_core::CallbackState::new(&nodes, core::array::from_fn(|id| id == cpu)) }
            .unwrap();
    let mut driver = state.claim_driver().unwrap();
    let mut marker = krcu_core::CallbackNode::new(complete_marker);
    // SAFETY: the unique stack marker stays fixed through detached invocation;
    // its callback does not free it. No protected data is associated with it.
    let accepted = unsafe { state.submit(cpu, &raw mut marker) };
    let started = driver.advance();
    let observe_fn = |_: &krcu_core::ReadGuard<'_>| {
        // SAFETY: current-task fixture under IRQ/preemption exclusion, with
        // stable storage through the safe-boundary cleanup below. This models
        // the outgoing-task hook without actually switching stacks.
        unsafe { state.note_context_switch(cpu, reader(&task)) }
    };
    // SAFETY: exclusive current-task fixture; no blocking or parallel scopes.
    // The final preempt-enable below must clean this retained membership.
    let reported_while_reading = unsafe { reader(&task).with_read_deferred(observe_fn) };
    let still_enrolled = reader(&task).is_enrolled();
    let needs_another_observation = state.needs_cpu_observation(cpu);
    // SAFETY: watched fixture, no bookkeeping lock held. A fresh report must
    // not mistake a finished but still linked reader for a completed period.
    let reported_before_cleanup = unsafe { state.report_ready(cpu) };
    let blocked_before_cleanup = driver.advance() && !state.has_ready(cpu);

    ktask::test_support::set_current_reschedule_pending(false);
    drop(guard);
    let cleaned = !reader(&task).is_enrolled();
    let requested_switch = ktask::test_support::is_current_reschedule_pending();
    ktask::test_support::set_current_reschedule_pending(was_pending);

    // SAFETY: still watched and IRQ-masked, no node lock held. This is only a
    // readiness recheck, not a second CPU observation or scheduler hook.
    let reported_after_cleanup = unsafe { state.report_ready(cpu) };
    let is_active = driver.advance();
    // SAFETY: sole executor, no bookkeeping locks, retained stack marker and
    // nonblocking callback. This fixture never retires real protected data.
    let invoked = unsafe { state.invoke_ready(cpu, 1) };

    // A regression in the tested boundary must not leave current's owner
    // pointing into this test's stack. Preserve every result above, then drain
    // the fixture independently before any test assertion can return early.
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: retained current reader, zero nesting, IRQ/preemption excluded;
        // the local nodes are still alive and no node lock is held.
        let cleanup = unsafe { ReaderCleanup::new(reader(&task)) };
        RcuContextIf::finish_reader_cleanup(cleanup);
        // SAFETY: watched fixture, fresh readiness check after fallback cleanup.
        unsafe { state.report_ready(cpu) };
        driver.advance();
        // SAFETY: the same retained marker and sole executor as above. If the
        // tested path already invoked it the queue is empty; otherwise the
        // fallback cleanup permits this independent invocation before return.
        unsafe { state.invoke_ready(cpu, 1) };
    }
    assert!(accepted && started);
    assert!(!reported_while_reading);
    assert!(still_enrolled);
    assert!(!needs_another_observation);
    assert!(!reported_before_cleanup);
    assert!(blocked_before_cleanup);
    assert!(cleaned);
    assert!(!requested_switch);
    assert!(reported_after_cleanup);
    assert!(!is_active);
    assert_eq!(invoked, 1);
}
