// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed RCU transport tests observe delivery without replacing the live action.
//! Per-CPU counters persist throughout this test boot; no service is stopped.

use core::sync::atomic::{AtomicUsize, Ordering};

use kirq::softirq::{SoftirqVec, local_softirq_pending, run_pending_softirqs};
use unittest::{assert_eq, def_test};

use super::{NotificationFlags, handle_bottom_half, notify_rcu};

static DELIVERIES: [AtomicUsize; kbuild_config::NR_CPUS] =
    [const { AtomicUsize::new(0) }; kbuild_config::NR_CPUS];

pub(super) fn record_delivery() {
    DELIVERIES[khal::percpu::this_cpu_id().as_usize()].fetch_add(1, Ordering::Release);
}

#[def_test(serial)]
fn rcu_reason_only_marks_pending_until_softirq_dispatch() {
    let _guard = kspin::NoPreemptIrqSave::new();
    {
        let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
        handle_bottom_half(NotificationFlags::RCU);
        assert_eq!(
            local_softirq_pending() & (1 << SoftirqVec::Rcu.as_usize()),
            1 << SoftirqVec::Rcu.as_usize()
        );
        let _ = run_pending_softirqs();
        assert_eq!(
            local_softirq_pending() & (1 << SoftirqVec::Rcu.as_usize()),
            1 << SoftirqVec::Rcu.as_usize()
        );
    }
    let _ = run_pending_softirqs();
}

#[def_test]
fn rcu_notification_rejects_absent_cpu() {
    assert_eq!(
        notify_rcu(kbuild_config::NR_CPUS.into()),
        Err(crate::KipiError::InvalidCpuId)
    );
}

// ktask/ipi enables these features together with boot-time fixed-IPI setup.
// The RCU reason alone does not ensure the transport has been enabled.
#[cfg(any(feature = "reschedule", feature = "timer-reprogram"))]
#[def_test(serial)]
fn rcu_self_ipi_is_deferred_with_irqs_masked() {
    let pin = kspin::NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id();
    let irq = kspin::IrqSave::new();
    let before = DELIVERIES[cpu.as_usize()].load(Ordering::Acquire);
    assert_eq!(notify_rcu(cpu), Ok(()));
    assert_eq!(DELIVERIES[cpu.as_usize()].load(Ordering::Acquire), before);
    drop(irq);
    // This generous deadline diagnoses lost delivery, not transport latency.
    // Reading the clock needs no task scheduling or timer callback.
    // Concurrent service requests may coalesce with ours; the real action stays
    // installed, so they are serviced normally on every CPU.
    let start_ns = khal::time::monotonic_time_nanos();
    while DELIVERIES[cpu.as_usize()].load(Ordering::Acquire) == before {
        if khal::time::monotonic_time_nanos().saturating_sub(start_ns) >= 5_000_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
    let was_delivered = DELIVERIES[cpu.as_usize()].load(Ordering::Acquire) != before;
    drop(pin);
    unittest::assert!(was_delivered, "RCU self IPI was not delivered within five seconds");
}
