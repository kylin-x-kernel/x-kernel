// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned injection of renewed requests at preemption-check guard release.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::TaskInner;

pub(super) struct Probe {
    checks: AtomicUsize,
    remaining: AtomicUsize,
    first_stack_address: AtomicUsize,
    max_stack_growth: AtomicUsize,
}

impl Probe {
    pub(super) const fn new() -> Self {
        Self {
            checks: AtomicUsize::new(0),
            remaining: AtomicUsize::new(0),
            first_stack_address: AtomicUsize::new(0),
            max_stack_growth: AtomicUsize::new(0),
        }
    }

    pub(super) fn record_check(&self) {
        self.checks.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn rearm(&self, task: &TaskInner) {
        // Only this task exercises its probe; no cross-task publication occurs.
        let remaining = self.remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            return;
        }
        let marker = 0u8;
        let address = core::hint::black_box(&marker as *const u8 as usize);
        let first = self.first_stack_address.load(Ordering::Relaxed);
        if first == 0 {
            self.first_stack_address.store(address, Ordering::Relaxed);
        } else {
            self.max_stack_growth
                .fetch_max(first.abs_diff(address), Ordering::Relaxed);
        }
        self.remaining.store(remaining - 1, Ordering::Relaxed);
        task.set_preempt_pending(true);
    }
}

struct ArmedProbe<'a>(&'a Probe);

impl Drop for ArmedProbe<'_> {
    fn drop(&mut self) {
        self.0.remaining.store(0, Ordering::Relaxed);
        self.0.first_stack_address.store(0, Ordering::Relaxed);
        self.0.max_stack_growth.store(0, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn irq_context_queries_do_not_enter_preemption_checks() {
    let task = crate::current();
    // Exclude real IRQ tails from the observation, without a preemption credit
    // that could hide a query's own zero-count guard release.
    let _irq = kspin::IrqSave::new();
    assert!(task.can_preempt(0));
    let before = task.preempt_check_probe.checks.load(Ordering::Relaxed);
    let snapshot = kirq::context::irq_context_snapshot();
    let is_interrupt = kirq::context::is_in_interrupt_context();
    let is_exception = khal::context::in_exception_context();
    let _pending = kirq::softirq::local_softirq_pending();
    let after = task.preempt_check_probe.checks.load(Ordering::Relaxed);
    assert_eq!(after, before);
    assert_eq!(is_interrupt, snapshot.is_in_interrupt_context());
    assert!(!is_exception);
    assert!(task.can_preempt(0));
    assert!(!karch::local_irq_enabled());
}

#[def_test(serial)]
fn irq_exclusion_defers_preemption_after_inner_guard_release() {
    let task = crate::current();
    let probe = &task.preempt_check_probe;
    let _fixture = ArmedProbe(probe);
    let irq = kspin::IrqSave::new();
    let was_pending = task.is_preempt_pending();
    {
        let _guard = kspin::NoPreempt::new();
        probe.remaining.store(1, Ordering::Relaxed);
        task.set_preempt_pending(true);
    }
    // The probe is consumed only after entering the reschedule path. Unlike
    // observing a peer task, this does not depend on scheduler selection.
    let remaining = probe.remaining.load(Ordering::Relaxed);
    let remains_pending = task.is_preempt_pending();
    probe.remaining.store(0, Ordering::Relaxed);
    task.set_preempt_pending(was_pending);
    drop(irq);
    assert_eq!(remaining, 1);
    assert!(remains_pending);
}

#[def_test(serial)]
fn irq_tail_keeps_outer_preemption_guard_effective() {
    let task = crate::current();
    let probe = &task.preempt_check_probe;
    let _fixture = ArmedProbe(probe);
    let irq = kspin::IrqSave::new();
    let was_pending = task.is_preempt_pending();
    task.disable_preempt();
    let tail_guard = kspin::NoPreempt::new();
    probe.remaining.store(1, Ordering::Relaxed);
    task.set_preempt_pending(true);
    tail_guard.release_without_check();
    let outer_credit_retained = task.can_preempt(1);
    TaskInner::current_check_preempt_pending_at_irq_exit();
    let remaining = probe.remaining.load(Ordering::Relaxed);
    let remains_pending = task.is_preempt_pending();
    probe.remaining.store(0, Ordering::Relaxed);
    task.set_preempt_pending(was_pending);
    task.enable_preempt(false);
    drop(irq);
    assert_eq!(remaining, 1);
    assert!(remains_pending);
    assert!(outer_credit_retained);
}

#[def_test(serial)]
fn irq_guard_release_defers_all_checks_to_explicit_tail() {
    let task = crate::current();
    let probe = &task.preempt_check_probe;
    let _fixture = ArmedProbe(probe);
    let irq = kspin::IrqSave::new();
    let was_pending = task.is_preempt_pending();
    let guard = kspin::NoPreempt::new();
    probe.remaining.store(1, Ordering::Relaxed);
    task.set_preempt_pending(true);
    let before = probe.checks.load(Ordering::Relaxed);
    guard.release_without_check();
    let after_release = probe.checks.load(Ordering::Relaxed);
    let is_balanced = task.can_preempt(0);
    let is_pending = task.is_preempt_pending();
    // Exercise the same provider as the HAL tail, after its guard was consumed.
    kiface_task_api::TaskIrqExitCheckpointIf::check_preempt_pending();
    let after_tail = probe.checks.load(Ordering::Relaxed);
    let remaining = probe.remaining.load(Ordering::Relaxed);
    let is_irq_masked = !karch::local_irq_enabled();
    task.set_preempt_pending(was_pending);
    drop(irq);
    assert_eq!(after_release, before);
    assert!(is_balanced);
    assert!(is_pending);
    assert!(after_tail > after_release);
    assert_eq!(remaining, 0);
    assert!(is_irq_masked);
}

#[def_test(serial)]
fn renewed_preemption_requests_do_not_accumulate_stack_frames() {
    let task = crate::current();
    let probe = &task.preempt_check_probe;
    let _fixture = ArmedProbe(probe);
    // Model the explicit IRQ-tail checkpoint under IRQ exclusion. Normal
    // guard release must leave these requests pending. Excluding hardware IRQs
    // keeps unrelated trap frames out of the stack-depth measurement.
    let _irq = kspin::IrqSave::new();
    assert!(task.can_preempt(0));
    {
        let _guard = kspin::NoPreempt::new();
        probe.remaining.store(256, Ordering::Relaxed);
        task.set_preempt_pending(true);
    }
    TaskInner::current_check_preempt_pending_at_irq_exit();
    assert_eq!(probe.remaining.load(Ordering::Relaxed), 0);
    assert!(probe.first_stack_address.load(Ordering::Relaxed) != 0);
    assert!(
        probe.max_stack_growth.load(Ordering::Relaxed) < 1024,
        "renewed requests accumulated preemption-check stack frames"
    );
    assert!(task.can_preempt(0));
}
