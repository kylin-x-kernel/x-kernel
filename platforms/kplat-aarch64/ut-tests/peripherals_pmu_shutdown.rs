// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

use unittest::{assert, assert_eq, def_test};

use super::shutdown_local_with;

static HAS_DELIVERED: AtomicBool = AtomicBool::new(false);

fn record_delivery() {
    HAS_DELIVERED.store(true, Ordering::Release);
}

#[def_test(serial)]
fn exclusive_periodic_source_can_stop_and_restart_on_this_cpu() -> unittest::TestResult {
    let guard = kspin::NoPreemptIrqSave::new();
    if kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return unittest::TestResult::Ignored;
    }
    // Never take over a live watchdog/perf counter. A dedicated configuration
    // with the watchdog off exercises real PMU/GIC shutdown and reuse.
    if super::PMU
        .get(&guard)
        .get()
        .is_some_and(|pmu| pmu.counters.iter().any(Option::is_some))
    {
        return unittest::TestResult::Ignored;
    }
    // Boot prepares/enables the PPI even when no watchdog counter was armed.
    assert!(kplat::nm_irq::shutdown_periodic_nmi());
    assert!(kplat::nm_irq::shutdown_periodic_nmi());
    for _ in 0..2 {
        HAS_DELIVERED.store(false, Ordering::Relaxed);
        assert!(kplat::nm_irq::enable_periodic_nmi(100_000, record_delivery));
        // The NMI source must actually deliver after two successful shutdowns;
        // a software counter-enabled flag cannot prove that the PPI was unmasked.
        // The runner supplies the external timeout for missing hardware delivery.
        while !HAS_DELIVERED.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        assert!(super::is_enabled(super::CYCLE_COUNTER_IDX));
        assert!(kplat::nm_irq::shutdown_periodic_nmi());
        assert!(kplat::nm_irq::shutdown_periodic_nmi());
        let pmu = super::PMU.get(&guard);
        let pmu = pmu.get().unwrap();
        assert!(pmu.counters[super::CYCLE_COUNTER_IDX as usize].is_none());
        assert!(pmu.overflow_handlers[super::CYCLE_COUNTER_IDX as usize].is_none());
        assert!(
            super::PERIODIC_IRQ_DISABLED
                .current(&guard)
                .load(Ordering::Relaxed)
        );
    }
    // Restore the startup-prepared, enabled PPI without taking a counter.
    khal::pmu::enable_irq();
    unittest::TestResult::Ok
}

#[def_test]
fn failed_disable_preserves_retry_without_teardown() {
    let disabled = AtomicBool::new(false);
    let trace = Cell::new(0);
    let stopped = shutdown_local_with(
        &disabled,
        || trace.set(trace.get() * 10 + 1),
        || {
            trace.set(trace.get() * 10 + 2);
            false
        },
        || {
            trace.set(trace.get() * 10 + 3);
            true
        },
    );
    assert!(!stopped);
    assert!(!disabled.load(Ordering::Relaxed));
    assert_eq!(trace.get(), 12);
}

#[def_test]
fn teardown_retry_does_not_acquire_another_disable_credit() {
    let disabled = AtomicBool::new(false);
    let trace = Cell::new(0);
    for succeeds in [false, true] {
        let stopped = shutdown_local_with(
            &disabled,
            || trace.set(trace.get() * 10 + 1),
            || {
                trace.set(trace.get() * 10 + 2);
                true
            },
            || {
                trace.set(trace.get() * 10 + 3);
                succeeds
            },
        );
        assert_eq!(stopped, succeeds);
        assert!(disabled.load(Ordering::Relaxed));
    }
    assert_eq!(trace.get(), 12313);
}

#[def_test]
fn successful_shutdown_orders_producer_before_controller() {
    let disabled = AtomicBool::new(false);
    let trace = Cell::new(0);
    let stopped = shutdown_local_with(
        &disabled,
        || trace.set(trace.get() * 10 + 1),
        || {
            trace.set(trace.get() * 10 + 2);
            true
        },
        || {
            trace.set(trace.get() * 10 + 3);
            true
        },
    );
    assert!(stopped);
    assert_eq!(trace.get(), 123);
}
