// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

use unittest::{assert, assert_eq, def_test};

use super::shutdown_local_with;

pub(super) const AFTER_PREPARATION: u8 = 1;
pub(super) const BEFORE_DELIVERY: u8 = 2;
pub(super) const DURING_TEARDOWN: u8 = 4;
pub(super) const CLOSE_ADMISSION: u8 = 8;

kcpu_slot::cpu_slot! {
    static FAILURES: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
}

pub(super) fn take_failure(stage: u8) -> bool {
    FAILURES.with_current_irq_disabled(|failures| {
        // A combined fault targets rollback, not the vacant-source shutdown
        // that precedes preparation in the same arm attempt.
        if stage == DURING_TEARDOWN
            && failures.load(Ordering::Relaxed) & (AFTER_PREPARATION | BEFORE_DELIVERY) != 0
        {
            return false;
        }
        failures.fetch_and(!stage, Ordering::Relaxed) & stage != 0
    })
}

struct FailureScope {
    pin: kspin::NoPreemptIrqSave,
    previous: u8,
    was_closed: bool,
}

impl FailureScope {
    fn new(stages: u8) -> Self {
        let pin = kspin::NoPreemptIrqSave::new();
        let previous = FAILURES.current(&pin).swap(stages, Ordering::Relaxed);
        Self {
            pin,
            previous,
            was_closed: !super::lifecycle::can_start(),
        }
    }
}

impl Drop for FailureScope {
    fn drop(&mut self) {
        FAILURES
            .current(&self.pin)
            .store(self.previous, Ordering::Relaxed);
        super::lifecycle::set_closed_for_test(self.was_closed);
    }
}

struct RestoreVacantSource;

impl Drop for RestoreVacantSource {
    fn drop(&mut self) {
        // FailureScope drops first, so injected failures cannot prevent cleanup
        // when a test assertion returns early. Real teardown failure stays closed.
        if super::shutdown_periodic_source() {
            khal::pmu::enable_irq();
        }
    }
}

static HAS_DELIVERED: AtomicBool = AtomicBool::new(false);
static HAS_REPLACED_CALLBACK: AtomicBool = AtomicBool::new(false);
static HAS_REJECTED_NMI_ARM: AtomicBool = AtomicBool::new(false);

fn record_delivery() {
    HAS_REJECTED_NMI_ARM.store(
        kirq::context::is_in_nmi()
            && !kplat::nm_irq::enable_periodic_nmi(200_000, rejected_delivery),
        Ordering::Relaxed,
    );
    HAS_DELIVERED.store(true, Ordering::Release);
}

fn rejected_delivery() {
    HAS_REPLACED_CALLBACK.store(true, Ordering::Release);
}

// This fixture owns event slot zero only. Keep its storage on failed controller
// teardown, just as the production periodic owner retains its cycle slot.
struct EventOwnerScope<'a> {
    pin: &'a kspin::NoPreemptIrqSave,
    is_installed: bool,
}

impl EventOwnerScope<'_> {
    fn cleanup(&mut self) -> bool {
        if !self.is_installed {
            return true;
        }
        super::with_counter(0, |counter| {
            counter.disable();
            let _ = counter.check_and_clear_overflow();
        });
        let desc = kirq::gic_level_irq_desc(of::pmu_irq_or(kbuild_config::PMU_IRQ));
        if !shutdown_local_with(
            super::PERIODIC_IRQ_DISABLED.current(self.pin),
            || {},
            || kirq::try_disable_irq_nosync(desc).is_ok(),
            || {
                // SAFETY: the fixture's sole producer is stopped; the retained
                // pin excludes migration/IRQs, and GIC teardown rejects active
                // claims. Failure leaves the event storage installed.
                unsafe { kirq::teardown_percpu_nmi(desc).is_ok() }
            },
        ) {
            return false;
        }
        // SAFETY: successful local teardown excludes NMI readers, the shared
        // counter borrow ended, and the fixture retains CPU/IRQ exclusion.
        let pmu = unsafe { super::PMU.get_mut_unchecked(self.pin) };
        pmu.counters[0] = None;
        pmu.overflow_handlers[0] = None;
        self.is_installed = false;
        true
    }
}

impl Drop for EventOwnerScope<'_> {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[def_test(serial)]
fn event_counter_owner_rejects_periodic_arm_and_shutdown_unchanged() -> unittest::TestResult {
    let pin = kspin::NoPreemptIrqSave::new();
    if kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None
        || !super::lifecycle::can_start()
        || super::PMU
            .get(&pin)
            .get()
            .is_some_and(|pmu| pmu.counters.iter().any(Option::is_some))
    {
        return unittest::TestResult::Ignored;
    }
    let counter = aarch64_pmuv3::pmuv3::PmuCounter::new_event_counter(
        0,
        u32::MAX as u64,
        aarch64_pmuv3::pmuv3::PmuEvent::CpuCycles,
    );
    if counter.check_pmu_support().is_err() {
        return unittest::TestResult::Ignored;
    }
    let _restore = RestoreVacantSource;
    counter.disable();
    let _ = counter.check_and_clear_overflow();
    assert!(super::shutdown_periodic_source());
    let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
    assert!(kplat::nm_irq::NmiMechanismIf::configure_nmi(irq));
    let mut owner = EventOwnerScope {
        pin: &pin,
        is_installed: false,
    };
    {
        // SAFETY: successful preparation verified the disabled/inactive PPI;
        // no counter is owned and this pin excludes task/ordinary IRQ mutation.
        let pmu = unsafe { super::PMU.get_mut_unchecked(&pin) };
        pmu.call_once(|| super::PmuManager {
            counters: [const { None }; super::MAX_PMU_COUNTERS],
            overflow_handlers: [const { None }; super::MAX_PMU_COUNTERS],
        });
        pmu.counters[0] = Some(counter);
        pmu.overflow_handlers[0] = Some(rejected_delivery);
        owner.is_installed = true;
    }
    super::enable(0);
    assert!(super::enable_periodic_irq(kirq::gic_level_irq_desc(irq)));
    assert!(super::is_enabled(0));
    assert!(
        !super::PERIODIC_IRQ_DISABLED
            .current(&pin)
            .load(Ordering::Relaxed)
    );
    for is_arm in [true, false] {
        let stages = if is_arm {
            AFTER_PREPARATION
        } else {
            DURING_TEARDOWN
        };
        let _failure = FailureScope::new(stages);
        let accepted = if is_arm {
            super::arm_periodic_source(100_000, record_delivery)
        } else {
            super::shutdown_periodic_source()
        };
        assert!(!accepted);
        // Neither controller preparation nor teardown was reached, and no
        // controller-disable credit was consumed by the rejected operation.
        assert_eq!(FAILURES.current(&pin).load(Ordering::Relaxed), stages);
        assert!(
            !super::PERIODIC_IRQ_DISABLED
                .current(&pin)
                .load(Ordering::Relaxed)
        );
        let pmu = super::PMU.get(&pin);
        let pmu = pmu.get().unwrap();
        let counter = pmu.counters[0].as_ref().unwrap();
        assert!(counter.is_enabled());
        assert_eq!(counter.counter_index(), 0);
        assert_eq!(counter.threshold(), u32::MAX as u64);
        assert!(core::ptr::fn_addr_eq(
            pmu.overflow_handlers[0].unwrap(),
            rejected_delivery as fn(),
        ));
        assert!(pmu.counters[1..].iter().all(Option::is_none));
        assert!(pmu.overflow_handlers[1..].iter().all(Option::is_none));
    }
    assert!(owner.cleanup());
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn periodic_setup_failures_preserve_retryable_ownership() -> unittest::TestResult {
    let pin = kspin::NoPreemptIrqSave::new();
    if kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None
        || super::PMU
            .get(&pin)
            .get()
            .is_some_and(|pmu| pmu.counters.iter().any(Option::is_some))
    {
        return unittest::TestResult::Ignored;
    }
    // Isolate this fixture from watchdog ownership and restore the boot PPI
    // state on success. The hook itself is scoped to this pinned CPU.
    let _restore = RestoreVacantSource;
    assert!(super::shutdown_periodic_source());
    for stage in [
        AFTER_PREPARATION,
        BEFORE_DELIVERY,
        AFTER_PREPARATION | DURING_TEARDOWN,
        BEFORE_DELIVERY | DURING_TEARDOWN,
        CLOSE_ADMISSION,
    ] {
        {
            let _failure = FailureScope::new(stage);
            assert!(!super::arm_periodic_source(100_000, record_delivery));
            assert_eq!(FAILURES.current(&pin).load(Ordering::Relaxed), 0);
        }
        let retains_counter = stage == (BEFORE_DELIVERY | DURING_TEARDOWN);
        assert_eq!(
            super::PMU
                .get(&pin)
                .get()
                .is_some_and(|pmu| { pmu.counters[super::CYCLE_COUNTER_IDX as usize].is_some() }),
            retains_counter
        );
        if retains_counter {
            assert!(!super::is_enabled(super::CYCLE_COUNTER_IDX));
            assert!(!super::arm_periodic_source(200_000, rejected_delivery));
        }
        assert!(super::shutdown_periodic_source());
        assert!(super::PMU.get(&pin).get().is_none_or(|pmu| {
            pmu.counters.iter().all(Option::is_none)
                && pmu.overflow_handlers.iter().all(Option::is_none)
        }));
        assert!(super::arm_periodic_source(100_000, record_delivery));
        // Install the teardown fault only after successful setup, so it cannot
        // be consumed by the initial vacant-source shutdown.
        {
            let _failure = FailureScope::new(DURING_TEARDOWN);
            assert!(!super::shutdown_periodic_source());
            assert_eq!(FAILURES.current(&pin).load(Ordering::Relaxed), 0);
        }
        let pmu = super::PMU.get(&pin);
        let pmu = pmu.get().unwrap();
        let counter = pmu.counters[super::CYCLE_COUNTER_IDX as usize]
            .as_ref()
            .unwrap();
        assert!(!counter.is_enabled());
        assert!(pmu.overflow_handlers[super::CYCLE_COUNTER_IDX as usize].is_some());
        assert!(!super::arm_periodic_source(200_000, rejected_delivery));
        assert!(super::shutdown_periodic_source());
        assert!(super::arm_periodic_source(100_000, record_delivery));
        assert!(super::shutdown_periodic_source());
    }
    unittest::TestResult::Ok
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
    // The first arm must establish exclusion itself, without a caller shutdown.
    assert!(kplat::nm_irq::enable_periodic_nmi(100_000, record_delivery));
    assert!(kplat::nm_irq::shutdown_periodic_nmi());
    assert!(kplat::nm_irq::shutdown_periodic_nmi());
    for _ in 0..2 {
        HAS_DELIVERED.store(false, Ordering::Relaxed);
        HAS_REPLACED_CALLBACK.store(false, Ordering::Relaxed);
        HAS_REJECTED_NMI_ARM.store(false, Ordering::Relaxed);
        assert!(kplat::nm_irq::enable_periodic_nmi(100_000, record_delivery));
        assert!(!kplat::nm_irq::enable_periodic_nmi(
            200_000,
            rejected_delivery
        ));
        {
            let pmu = super::PMU.get(&guard);
            let pmu = pmu.get().unwrap();
            let counter = pmu.counters[super::CYCLE_COUNTER_IDX as usize]
                .as_ref()
                .unwrap();
            assert_eq!(counter.threshold(), 250_000);
            assert!(core::ptr::fn_addr_eq(
                pmu.overflow_handlers[super::CYCLE_COUNTER_IDX as usize].unwrap(),
                record_delivery as fn(),
            ));
        }
        // The NMI source must actually deliver after two successful shutdowns;
        // a software counter-enabled flag cannot prove that the PPI was unmasked.
        // The runner supplies the external timeout for missing hardware delivery.
        while !HAS_DELIVERED.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        assert!(super::is_enabled(super::CYCLE_COUNTER_IDX));
        assert!(!HAS_REPLACED_CALLBACK.load(Ordering::Acquire));
        assert!(HAS_REJECTED_NMI_ARM.load(Ordering::Relaxed));
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
