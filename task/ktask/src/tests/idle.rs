// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Idle handshake regressions using real pending IRQs and one-shot timers.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kspin::{NoPreempt, NoPreemptIrqSave};
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::{assert, def_test};

#[cfg(all(target_arch = "aarch64", feature = "ipi"))]
fn arm_delivery_masks() -> (u64, Option<u64>) {
    let daif: u64;
    let features: u64;
    // SAFETY: These are read-only observations of privileged CPU state on
    // the current initialized EL1 CPU; neither instruction changes masks.
    unsafe {
        core::arch::asm!(
            "mrs {daif}, daif",
            "mrs {features}, ID_AA64PFR1_EL1",
            daif = out(reg) daif,
            features = out(reg) features,
            options(nomem, nostack, preserves_flags),
        );
    }
    const NMI_FEATURE_SHIFT: u32 = 36;
    let allint = if (features >> NMI_FEATURE_SHIFT) & 0xf == 1 {
        let value: u64;
        // SAFETY: The feature register above establishes FEAT_NMI, so ALLINT
        // is implemented. Its read does not enable or mask any interrupt.
        unsafe {
            core::arch::asm!(
                "mrs {}, s3_0_c4_c3_0", out(reg) value,
                options(nomem, nostack, preserves_flags),
            );
        }
        Some(value)
    } else {
        None
    };
    (daif, allint)
}

#[def_test(serial)]
fn retained_reschedule_skips_idle_without_consuming_request() {
    use unittest::assert_eq;

    let guard = NoPreemptIrqSave::new();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    let sequence = watching.sequence_for_test();
    let task = crate::current();
    let was_pending = task.is_preempt_pending();
    task.set_preempt_pending(true);
    let waited = super::idle_wait_if_no_work(&guard);
    let remains_pending = task.is_preempt_pending();
    task.set_preempt_pending(was_pending);
    assert!(!waited);
    assert!(remains_pending);
    assert_eq!(watching.sequence_for_test(), sequence);
}

#[cfg(all(feature = "ipi", feature = "smp"))]
#[def_test(serial)]
fn reschedule_notification_after_final_check_wakes_idle() {
    let _pin = NoPreempt::new();
    let task = crate::current().clone();
    let guard = NoPreemptIrqSave::new();
    let was_pending = task.is_preempt_pending();
    task.set_preempt_pending(false);
    khal::time::disarm_timer();
    // Model a completed no-work check, then inject without rechecking readiness.
    // This isolates the architecture half of the scheduler/CPU handshake.
    kipi::notify_reschedule(khal::percpu::this_cpu_id()).unwrap();
    // SAFETY: The guard masks IRQs and excludes migration; no lock is held.
    // The real IPI is pending, and the outer pin spans delivery after return.
    unsafe { karch::idle_wait() };
    let is_irq_disabled = !karch::local_irq_enabled();
    // ARM can wake from WFI before taking the IRQ; restoring delivery here
    // permits its handler to run. The outer pin still prevents scheduling
    // from consuming need_resched before we observe it.
    drop(guard);
    let limit_ns = khal::time::monotonic_time_nanos() + 2_000_000_000;
    while !task.is_preempt_pending() && khal::time::monotonic_time_nanos() < limit_ns {
        core::hint::spin_loop();
    }
    let delivered = task.is_preempt_pending();
    if was_pending {
        task.set_preempt_pending(true);
    }
    super::rearm_local_timer();
    assert!(is_irq_disabled);
    assert!(delivered);
}

#[cfg(feature = "ipi")]
#[def_test(serial)]
fn pending_timer_notification_wakes_idle_without_reschedule() {
    use unittest::assert_eq;

    let _pin = NoPreempt::new();
    let irq_was_enabled = karch::local_irq_enabled();
    let task = crate::current().clone();
    let fired = Arc::new(AtomicBool::new(false));
    let callback_fired = fired.clone();
    let callback_was_watched = Arc::new(AtomicBool::new(false));
    let callback_watching = callback_was_watched.clone();
    // Core registration publishes the deadline without programming hardware;
    // the notification requests arbitration through the real IRQ path.
    let timer = ktimer_core::Timer::new(
        &crate::timers::PREPARED_TIMERS,
        move |_: crate::TimerControl<'_>, _: crate::Expiration| {
            callback_watching.store(
                krcu_core::RcuContextIf::current_cpu_watching().is_watching(),
                Ordering::Release,
            );
            callback_fired.store(true, Ordering::Release);
        },
    );
    let guard = NoPreemptIrqSave::new();
    let cpu = khal::percpu::this_cpu_id();
    let was_pending = task.is_preempt_pending();
    task.set_preempt_pending(false);
    let deadline_ns = khal::time::monotonic_time_nanos() + 2_000_000;
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(deadline_ns));
    // SAFETY: This CPU and its static timer service are initialized and pinned;
    // owned storage outlives delivery/cancel, and the callback only sets a flag.
    unsafe { timer.start_on(cpu, deadline) }.unwrap();
    // Stop future timer expiry while checking notification-driven progress.
    // This does not clear an already pending IRQ, so the test establishes
    // delivery, not exclusive attribution to this IPI. Cleanup rearms normally.
    khal::time::disarm_timer();
    kipi::notify_timer_reprogram(cpu).unwrap();
    let mask = karch::save_irq_and_disable();
    #[cfg(target_arch = "aarch64")]
    let arm_masks = arm_delivery_masks();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    let sequence = watching.sequence_for_test();
    // No task was made runnable: the pending hardware interrupt, not the
    // need_resched flag, must prevent indefinite sleep. The host runner must bound
    // the run because a broken wait cannot check a guest-side timeout.
    let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
        // SAFETY: retained current fixture under IRQ/preemption exclusion;
        // the idle boundary below removes membership from this static node.
        unsafe {
            krcu_core::RcuContextIf::reader_node_for_tests(cpu.as_usize()).enroll(&task.rcu_reader)
        };
    };
    // SAFETY: exclusive current-task fixture, cleaned by the idle boundary.
    unsafe { task.rcu_reader.with_read_deferred(enroll_fn) };
    let was_enrolled = task.rcu_reader.is_enrolled();
    let waited = super::idle_wait_if_no_work(&guard);
    let cleaned_before_guard_release = !task.rcu_reader.is_enrolled();
    let returned_sequence = watching.sequence_for_test();
    let is_watched_after_wait = watching.is_watching();
    let is_irq_disabled = !karch::local_irq_enabled();
    let returned_mask = karch::save_irq_and_disable();
    #[cfg(target_arch = "aarch64")]
    let returned_arm_masks = arm_delivery_masks();
    // Preserve a request raised by an unrelated IRQ during the x86/LoongArch
    // wait rather than clearing production state while cleaning up the test.
    if was_pending {
        task.set_preempt_pending(true);
    }
    drop(guard);
    while !fired.load(Ordering::Acquire)
        && khal::time::monotonic_time_nanos() < deadline_ns + 2_000_000_000
    {
        core::hint::spin_loop();
    }
    let delivered = fired.load(Ordering::Acquire);
    timer.cancel_sync();
    super::rearm_local_timer();
    assert!(waited);
    assert!(was_enrolled);
    assert!(cleaned_before_guard_release);
    assert!(delivered);
    assert!(callback_was_watched.load(Ordering::Acquire));
    assert!(is_watched_after_wait);
    // The idle pair alone advances twice. IRQs delivered inside the wait can
    // add crossings; ARM may instead deliver them after the guard is released.
    assert!(returned_sequence.wrapping_sub(sequence) >= 2);
    assert!(is_irq_disabled);
    assert_eq!(returned_mask, mask);
    #[cfg(target_arch = "aarch64")]
    assert_eq!(returned_arm_masks, arm_masks);
    assert_eq!(karch::local_irq_enabled(), irq_was_enabled);
    assert!(crate::current().ptr_eq(&task));
}

#[def_test(serial)]
fn one_shot_irq_wakes_idle_and_preserves_caller_context() {
    assert!(karch::local_irq_enabled());
    let _pin = NoPreempt::new();
    let task = crate::current().clone();
    let fired = Arc::new(AtomicBool::new(false));
    let callback_fired = fired.clone();
    let timer = crate::Timer::new(move |_: crate::TimerControl<'_>, _: crate::Expiration| {
        callback_fired.store(true, Ordering::Release);
    });
    let deadline_ns = khal::time::monotonic_time_nanos() + 2_000_000;
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(deadline_ns));
    let initial_guard = NoPreemptIrqSave::new();
    // SAFETY: The CPU is initialized and pinned; timer/callback storage stays
    // live until cancel_sync. Only the real timer IRQ dispatches the callback.
    unsafe { timer.start(deadline) }.unwrap();
    // SAFETY: The initial guard prevents consuming the one-shot before this
    // first wait. No scheduler lock is held; preemption remains excluded.
    unsafe { karch::idle_wait() };
    let mut is_irq_state_preserved = !karch::local_irq_enabled();
    drop(initial_guard);
    while !fired.load(Ordering::Acquire) {
        let guard = NoPreemptIrqSave::new();
        if !fired.load(Ordering::Acquire) {
            // SAFETY: IRQs and preemption are disabled, no lock is held, and
            // task-context NMI state remains intact. The one-shot is armed.
            unsafe { karch::idle_wait() };
            is_irq_state_preserved &= !karch::local_irq_enabled();
        }
        drop(guard);
        if khal::time::monotonic_time_nanos() > deadline_ns + 2_000_000_000 {
            break;
        }
    }
    timer.cancel_sync();
    assert!(fired.load(Ordering::Acquire));
    assert!(is_irq_state_preserved);
    assert!(karch::local_irq_enabled());
    assert!(crate::current().ptr_eq(&task));
}
