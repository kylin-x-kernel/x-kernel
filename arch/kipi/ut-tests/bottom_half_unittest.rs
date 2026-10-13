// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serial softirq transport probes; scoped actions restore the boot registrations.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use kirq::softirq::{
    SoftirqVec, local_softirq_pending, run_pending_softirqs,
    test_support::{ScopedSoftirqAction, begin_softirq_test},
};
use unittest::{assert_eq, def_test};

use super::{
    NotificationFlags, handle_bottom_half, notify_bottom_half, notify_highpri_bottom_half,
};

static NORMAL_CALLS: AtomicUsize = AtomicUsize::new(0);
static HIGH_CALLS: AtomicUsize = AtomicUsize::new(0);
static TARGET: AtomicUsize = AtomicUsize::new(usize::MAX);

fn normal_action() {
    core::assert!(kirq::context::is_softirq_context());
    core::assert_eq!(
        khal::percpu::this_cpu_id().as_usize(),
        TARGET.load(Ordering::Relaxed)
    );
    NORMAL_CALLS.fetch_add(1, Ordering::Release);
}

fn high_action() {
    core::assert!(kirq::context::is_softirq_context());
    core::assert_eq!(
        khal::percpu::this_cpu_id().as_usize(),
        TARGET.load(Ordering::Relaxed)
    );
    HIGH_CALLS.fetch_add(1, Ordering::Release);
}

fn high_action_with_continuation() {
    // The softirq runner serializes this action on its target CPU. Publish the
    // observation only after issuing the new request during the first pass.
    core::assert!(kirq::context::is_softirq_context());
    let cpu = khal::percpu::this_cpu_id();
    core::assert_eq!(cpu.as_usize(), TARGET.load(Ordering::Relaxed));
    if HIGH_CALLS.load(Ordering::Relaxed) == 0 {
        core::assert_eq!(notify_highpri_bottom_half(cpu), Ok(()));
    }
    HIGH_CALLS.fetch_add(1, Ordering::Release);
}

struct Actions {
    _normal: ScopedSoftirqAction,
    _high: ScopedSoftirqAction,
}

impl Actions {
    fn new(cpu: LogicalCpuId, high: fn()) -> Self {
        TARGET.store(cpu.as_usize(), Ordering::Relaxed);
        NORMAL_CALLS.store(0, Ordering::Relaxed);
        HIGH_CALLS.store(0, Ordering::Relaxed);
        Self {
            _normal: ScopedSoftirqAction::install(SoftirqVec::Tasklet, normal_action),
            _high: ScopedSoftirqAction::install(SoftirqVec::High, high),
        }
    }
}

/// Fixed BH reasons only mark pending; callbacks require a later softirq pass.
///
/// # Panics
/// Panics on wrong vector routing, inline execution or wrong callback context.
#[def_test(serial)]
fn bottom_half_delivery_defers_callbacks_and_preserves_categories() {
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _actions = Actions::new(khal::percpu::this_cpu_id(), high_action);
    let normal_bit = 1 << SoftirqVec::Tasklet.as_usize();
    let high_bit = 1 << SoftirqVec::High.as_usize();
    let mask = normal_bit | high_bit;
    {
        let _hardirq = kirq::context::test_support::ScopedHardIrqContext::enter();
        handle_bottom_half(NotificationFlags::BOTTOM_HALF);
        assert_eq!(local_softirq_pending() & mask, normal_bit);
        handle_bottom_half(NotificationFlags::HIGHPRI_BOTTOM_HALF);
        assert_eq!(local_softirq_pending() & mask, mask);
        assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 0);
        assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 0);
        let _ = run_pending_softirqs();
        assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 0);
        assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 0);
    }
    let _ = run_pending_softirqs();
    assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 1);
    assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 1);
    assert_eq!(local_softirq_pending() & mask, 0);
}

/// Invalid destinations are rejected before either BH reason can be published.
///
/// # Panics
/// Panics if an out-of-range or absent CPU is accepted.
#[def_test]
fn bottom_half_notification_rejects_invalid_cpu() {
    for index in 0..=kbuild_config::NR_CPUS {
        let cpu = LogicalCpuId::new(index);
        if index == kbuild_config::NR_CPUS || raw_cpu_id(cpu).is_none() {
            assert_eq!(notify_bottom_half(cpu), Err(crate::KipiError::InvalidCpuId));
            assert_eq!(
                notify_highpri_bottom_half(cpu),
                Err(crate::KipiError::InvalidCpuId)
            );
        }
    }
}

/// A real remote IPI reaches both BH categories and retains an in-pass re-kick.
///
/// This transport integration probe waits for explicit callback acknowledgments,
/// not a latency threshold. The outer QEMU timeout diagnoses missing progress.
/// Serial execution and quiescent production BH queues are required while the
/// scoped actions replace the boot handlers. No generic callback is enqueued.
///
/// # Panics
/// Panics on delivery error, wrong CPU/context or duplicate callback execution.
#[def_test(serial)]
fn bottom_half_remote_ipi_and_in_pass_continuation() {
    let _cpu = kspin::NoPreempt::new();
    let local = khal::percpu::this_cpu_id();
    let mut remote = None;
    for_each_present_logical_cpu(|_, cpu, _| {
        if cpu != local && remote.is_none() {
            remote = Some(cpu);
        }
    });
    let Some(remote) = remote else {
        return unittest::TestResult::Ignored;
    };
    let _wake_gate = begin_softirq_test();
    let _actions = Actions::new(remote, high_action_with_continuation);
    // Exercise the sender with IRQs disabled, as in a prepared IRQ submission.
    {
        let _irq = kspin::IrqSave::new();
        assert_eq!(notify_bottom_half(remote), Ok(()));
        assert_eq!(notify_highpri_bottom_half(remote), Ok(()));
    }
    while NORMAL_CALLS.load(Ordering::Acquire) == 0 || HIGH_CALLS.load(Ordering::Acquire) < 2 {
        core::hint::spin_loop();
    }
    assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 1);
    assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 2);
}

/// Self notification uses deferred IPI delivery even with only one boot CPU.
///
/// # Panics
/// Panics on inline execution, notification error or duplicate delivery.
#[def_test(serial)]
fn bottom_half_self_ipi_does_not_execute_inline() {
    let _cpu = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    // A non-IPI runtime can link the local BH adapter without enabling IPIs.
    if super::PENDING[cpu.as_usize()].0.load(Ordering::Acquire) & NotificationFlags::ENABLED.bits()
        == 0
    {
        assert_eq!(
            cfg!(any(feature = "reschedule", feature = "timer-reprogram")),
            false
        );
        return unittest::TestResult::Ignored;
    }
    let _wake_gate = begin_softirq_test();
    let _actions = Actions::new(cpu, high_action);
    {
        let _irq = kspin::IrqSave::new();
        assert_eq!(notify_bottom_half(cpu), Ok(()));
        assert_eq!(notify_highpri_bottom_half(cpu), Ok(()));
        assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 0);
        assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 0);
    }
    while NORMAL_CALLS.load(Ordering::Acquire) == 0 || HIGH_CALLS.load(Ordering::Acquire) == 0 {
        core::hint::spin_loop();
    }
    assert_eq!(NORMAL_CALLS.load(Ordering::Acquire), 1);
    assert_eq!(HIGH_CALLS.load(Ordering::Acquire), 1);
}
