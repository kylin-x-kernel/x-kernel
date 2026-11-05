// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "nmi-pmu")]
use core::sync::atomic::{AtomicBool, Ordering};

use aarch64_pmuv3::pmuv3::{PmuCounter, PmuEvent};
type PerfCb = fn();
const MAX_PMU_COUNTERS: usize = 32;

#[cfg(feature = "nmi-pmu")]
pub(crate) mod lifecycle;

/// Index of the PMU cycle counter (PMCCNTR), used by the periodic NMI backend.
pub const CYCLE_COUNTER_IDX: u32 = (MAX_PMU_COUNTERS - 1) as u32;

pub struct PmuManager {
    counters: [Option<PmuCounter>; MAX_PMU_COUNTERS],
    overflow_handlers: [Option<PerfCb>; MAX_PMU_COUNTERS],
}
// SAFETY: `LazyInit::new()` is an uninitialized marker without a payload or
// owned resource. Each CPU initializes its copied marker independently.
kcpu_slot::cpu_slot_cell_with_template! {
    static PMU: lazyinit::LazyInit<crate::peripherals::pmu::PmuManager> =
        unsafe { lazyinit::LazyInit::new() };
}

// Boot prepares the PPI before any counter manager exists. Retrying local
// teardown must not initialize or mutably borrow NMI-visible counter storage.
#[cfg(feature = "nmi-pmu")]
kcpu_slot::cpu_slot! {
    static PERIODIC_IRQ_DISABLED: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
}

/// Runs `f` with the current CPU's initialized [`PmuManager`], initialising it
/// on first access.
///
/// The guard pins the current CPU and masks local IRQs for the call; the
/// caller-scoped mutable reference cannot escape.
#[inline]
fn with_pmu_inited<R>(f: impl FnOnce(&mut PmuManager) -> R) -> R {
    let guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: the guard pins this CPU with local IRQs masked, and only this
    // call mutates the manager.
    let pmu = unsafe { PMU.get_mut_unchecked(&guard) };
    pmu.call_once(|| PmuManager {
        counters: [const { None }; MAX_PMU_COUNTERS],
        overflow_handlers: [const { None }; MAX_PMU_COUNTERS],
    });
    f(pmu)
}

pub fn register_overflow_handler(index: u32, handler: PerfCb) -> bool {
    let idx = index as usize;
    if idx >= MAX_PMU_COUNTERS {
        warn!("register_overflow_handler: index {idx} out of range");
        return false;
    }
    with_pmu_inited(|pmu| {
        #[cfg(feature = "nmi-pmu")]
        if !lifecycle::can_start() {
            return false;
        }
        if pmu.counters[idx].is_none() {
            warn!("register_overflow_handler: counter {idx} not initialised");
            return false;
        }
        pmu.overflow_handlers[idx] = Some(handler);
        true
    })
}

pub fn init_cycle_counter(threshold: u64) -> bool {
    with_pmu_inited(|pmu| {
        #[cfg(feature = "nmi-pmu")]
        if !lifecycle::can_start() {
            return false;
        }
        let idx = MAX_PMU_COUNTERS - 1;
        if pmu.counters[idx].is_some() {
            return false;
        }
        let counter = PmuCounter::new_cycle_counter(threshold);
        if counter.check_pmu_support().is_err() {
            return false;
        }
        pmu.counters[idx] = Some(counter);
        true
    })
}

/// Release this CPU's cycle counter and its overflow handler.
///
/// Rolls back a partially failed [`enable_periodic_nmi`]: the counter is
/// disabled (if it was ever started) and both the counter and handler slots
/// are cleared so a later arming attempt starts from a clean per‑CPU state.
pub fn deinit_cycle_counter() {
    with_pmu_inited(|pmu| {
        if let Some(Some(counter)) = pmu.counters.get_mut(CYCLE_COUNTER_IDX as usize) {
            counter.disable();
        }
        pmu.counters[CYCLE_COUNTER_IDX as usize] = None;
        pmu.overflow_handlers[CYCLE_COUNTER_IDX as usize] = None;
    });
}

pub fn init_event_counter(index: u32, threshold: u64, event: PmuEvent) -> bool {
    let idx = index as usize;
    if idx >= MAX_PMU_COUNTERS - 1 {
        return false;
    }
    with_pmu_inited(|pmu| {
        #[cfg(feature = "nmi-pmu")]
        if !lifecycle::can_start() {
            return false;
        }
        if pmu.counters[idx].is_some() {
            return false;
        }
        let counter = PmuCounter::new_event_counter(index, threshold, event);
        if counter.check_pmu_support().is_err() {
            return false;
        }
        pmu.counters[idx] = Some(counter);
        true
    })
}
/// Runs a closure with a mutable reference to the PMU counter at `index` on the
/// current CPU.
///
/// `index` must have been initialized via [`init_cycle_counter`] or
/// [`init_event_counter`]; an uninitialized counter is a no-op (the closure is
/// silently skipped). The guard pins the current CPU for the call.
#[inline]
fn with_counter_mut<F>(index: u32, f: F)
where
    F: FnOnce(&mut PmuCounter),
{
    let guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: the guard pins this CPU with local IRQs masked; this call is the
    // only mutable access to the per-CPU manager.
    let pmu = unsafe { PMU.get_mut_unchecked(&guard) };
    if let Some(Some(counter)) = pmu.counters.get_mut(index as usize) {
        f(counter);
    }
}

/// Runs a closure with a shared reference to the PMU counter at `index` on the
/// current CPU.
///
/// Counter slots are only changed during per-CPU setup or rollback before a
/// source is enabled; once armed, the [`PmuCounter`] methods used here take
/// `&self`, so they may race safely with PMU IRQ/NMI dispatch.
#[inline]
fn with_counter<F>(index: u32, f: F)
where
    F: FnOnce(&PmuCounter),
{
    let guard = kspin::NoPreemptIrqSave::new();
    let Some(pmu) = PMU.get(&guard).get() else {
        return;
    };
    if let Some(Some(counter)) = pmu.counters.get(index as usize) {
        f(counter);
    }
}

pub fn enable(index: u32) {
    // `PmuCounter::enable` only mutates its atomic enabled state and PMU
    // registers, so shared access remains valid if an NMI observes it.
    with_counter(index, |c| {
        #[cfg(feature = "nmi-pmu")]
        if !lifecycle::can_start() {
            return;
        }
        c.enable();
    });
}
pub fn disable(index: u32) {
    // As `enable`; the terminal NMI quiesce path may race a pending overflow.
    with_counter(index, |c| c.disable());
}
pub fn is_enabled(index: u32) -> bool {
    let mut enabled = false;
    with_counter(index, |counter| enabled = counter.is_enabled());
    enabled
}
pub fn handle_overflows() -> bool {
    // Called from PMU IRQ/NMI context on the current CPU. Active counters
    // expose their state through `&self` plus atomics, so shared access also
    // permits the terminal quiesce path to race with a pending overflow.
    let guard = kspin::NoPreemptIrqSave::new();
    let Some(pmu) = PMU.get(&guard).get() else {
        return false;
    };
    let mut dispatched_any = false;
    for idx in 0..MAX_PMU_COUNTERS {
        let handler = pmu.overflow_handlers[idx];
        let Some(counter) = pmu.counters[idx].as_ref() else {
            continue;
        };
        if counter.handle_overflow().is_ok() {
            dispatched_any = true;
            if let Some(handler) = handler {
                handler();
            }
        }
    }
    dispatched_any
}
pub fn set_threshold(index: u32, threshold: u64) {
    with_counter_mut(index, |c| c.set_threshold(threshold));
}

/// Stops the exclusive local periodic source without removing the Line owner.
///
/// Implements [`kplat::nm_irq::PeriodicNmiSourceIf::shutdown_periodic_nmi`]:
/// the caller owns shutdown/restart serialization. Rejects NMI, unavailable
/// mechanisms and any allocated event counter. A startup-prepared PPI without
/// a cycle counter still needs disable/teardown. False
/// retains local storage for retry; true permits later local setup, not global
/// handler destruction without completed shutdown on the other participants.
#[cfg(feature = "nmi-pmu")]
pub fn shutdown_periodic_source() -> bool {
    if kirq::context::is_in_nmi() || kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return false;
    }
    let guard = kspin::NoPreemptIrqSave::new();
    let pmu = PMU.get(&guard).get();
    // Disabling the PPI would also suppress event-counter consumers. No state
    // changes are permitted unless this source owns the whole local PMU line.
    if pmu.is_some_and(|pmu| {
        pmu.counters[..CYCLE_COUNTER_IDX as usize]
            .iter()
            .any(Option::is_some)
    }) {
        return false;
    }
    let counter = pmu.and_then(|pmu| pmu.counters[CYCLE_COUNTER_IDX as usize].as_ref());
    let is_irq_disabled = PERIODIC_IRQ_DISABLED.current(&guard);
    let desc = kirq::gic_level_irq_desc(of::pmu_irq_or(kbuild_config::PMU_IRQ));
    let stopped = shutdown_local_with(
        is_irq_disabled,
        || {
            if let Some(counter) = counter {
                counter.disable();
                // NoOverflow also means there is no stale event to clear.
                let _ = counter.check_and_clear_overflow();
            }
        },
        || kirq::try_disable_irq_nosync(desc).is_ok(),
        || {
            // SAFETY: this CPU is pinned, its sole producer is stopped, and
            // ordinary IRQs stay masked. The GIC teardown verifies disable
            // completion, rejects active claims and clears pending delivery;
            // failure retains storage, so no claim can cross successful reuse.
            unsafe { kirq::teardown_percpu_nmi(desc).is_ok() }
        },
    );
    if !stopped {
        return false;
    }
    if pmu.is_some() {
        // SAFETY: the producer and local PPI are stopped, pending/active delivery
        // has been excluded by successful teardown, and no other counters exist.
        // The guard excludes local task/IRQ access; the shared borrow above is over.
        let pmu = unsafe { PMU.get_mut_unchecked(&guard) };
        pmu.counters[CYCLE_COUNTER_IDX as usize] = None;
        pmu.overflow_handlers[CYCLE_COUNTER_IDX as usize] = None;
    }
    // Teardown does not consume the disable credit. Retain it through repeated
    // successful shutdowns; only a successful local rearm consumes it.
    true
}

#[cfg(feature = "nmi-pmu")]
pub(crate) fn enable_periodic_irq(desc: kirq::IrqDesc) -> bool {
    let guard = kspin::NoPreemptIrqSave::new();
    if kirq::try_enable(desc, true).is_err() {
        return false;
    }
    PERIODIC_IRQ_DISABLED
        .current(&guard)
        .store(false, Ordering::Relaxed);
    true
}

#[cfg(feature = "nmi-pmu")]
fn shutdown_local_with(
    is_irq_disabled: &AtomicBool,
    stop: impl FnOnce(),
    disable: impl FnOnce() -> bool,
    teardown: impl FnOnce() -> bool,
) -> bool {
    stop();
    if !is_irq_disabled.load(Ordering::Relaxed) {
        if !disable() {
            return false;
        }
        // Retry teardown without consuming another nested disable credit.
        is_irq_disabled.store(true, Ordering::Relaxed);
    }
    teardown()
}

#[cfg(feature = "nmi-pmu")]
unittest::path_mod_test!(tests_shutdown, "../../ut-tests/peripherals_pmu_shutdown.rs");
#[macro_export]
macro_rules! pmu_if_impl {
    () => {
        #[kiface::provide]
        impl kplat::perf::PmuInterruptIf {
            fn shutdown_overflow_irq() -> kerrno::KResult<()> {
                #[cfg(feature = "nmi-pmu")]
                {
                    $crate::peripherals::pmu::lifecycle::shutdown()
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    Err(kerrno::KError::Unsupported)
                }
            }

            fn restore_overflow_irq() -> kerrno::KResult<()> {
                #[cfg(feature = "nmi-pmu")]
                {
                    $crate::peripherals::pmu::lifecycle::restore()
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    Err(kerrno::KError::Unsupported)
                }
            }

            fn register_overflow_irq() -> bool {
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                let desc = kirq::gic_level_irq_desc(irq);
                // The platform owns this mapping independently of the normal
                // or NMI action. Preparing it does not enable PMU delivery.
                if let Err(err) = kirq::try_map(desc) {
                    log::warn!("PMU mapping preparation failed: desc={desc:?}, error={err:?}");
                    return false;
                }
                let handler = alloc::sync::Arc::new(|_| {
                    $crate::peripherals::pmu::handle_overflows();
                    kirq::IrqEvent::HANDLED
                });
                // Normal IRQ delivery in pmu-only builds; NMI registration
                // (one Line-owned action, also used by IRQ fallback) when the PMU is the
                // compiled NMI source, so the line can later be promoted.
                #[cfg(feature = "nmi-pmu")]
                {
                    kirq::register_nmi_startup(desc, handler)
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    match kirq::try_register_startup(
                        desc,
                        handler,
                        kirq::IrqRegisterOptions::auto_enable(),
                    ) {
                        Ok(true) => true,
                        Ok(false) => {
                            log::warn!("PMU registration rejected: desc={desc:?}");
                            false
                        }
                        Err(err) => {
                            log::warn!("PMU registration failed: desc={desc:?}, error={err:?}");
                            false
                        }
                    }
                }
            }

            fn enable_irq() {
                let _pin = kspin::NoPreemptIrqSave::new();
                #[cfg(feature = "nmi-pmu")]
                if !$crate::peripherals::pmu::lifecycle::can_start() {
                    return;
                }
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                #[cfg(feature = "nmi-pmu")]
                if kplat::nm_irq::NmiMechanismIf::mode() != kplat::nm_irq::NmiMode::None
                    && !kplat::nm_irq::NmiMechanismIf::configure_nmi(irq)
                {
                    log::warn!(
                        "PMU IRQ remains disabled after local NMI preparation failure: {irq}"
                    );
                    return;
                }
                // Use the same descriptor as the registration so the enable
                // targets the same resolved line (bare numbers would fall
                // back to the PlainVirq identity path).
                let desc = kirq::gic_level_irq_desc(irq);
                #[cfg(feature = "nmi-pmu")]
                if !$crate::peripherals::pmu::enable_periodic_irq(desc) {
                    log::warn!("PMU local IRQ enable failed: {irq}");
                }
                #[cfg(not(feature = "nmi-pmu"))]
                kirq::enable(desc, true);
            }
        }
    };
}

/// Implements the source‑neutral [`kplat::nm_irq::PeriodicNmiSourceIf`] interface
/// using the PMU cycle counter as the NMI source.
///
/// This is the PMU backend of the NMI subsystem: it computes the overflow
/// threshold, promotes the PMU IRQ line to NMI delivery, initialises the
/// cycle counter, and registers the consumer callback.  The line's
/// overflow‑dispatch handler is registered by
/// `khal::pmu::register_overflow_dispatch` (called from the `pmu` feature
/// init) independently of the watchdog — as a normal IRQ handler in pmu-only
/// builds, or as an NMI handler (with normal‑IRQ fallback) when the PMU is
/// the compiled NMI source (`nmi-pmu`) — so perf overflow delivery works
/// even without the hardlockup watchdog.  Consumers never see these details;
/// they only depend on `khal::nmi::enable_periodic_nmi`.
///
/// # Single‑provider note
///
/// `PeriodicNmiSourceIf` allows exactly one provider.  As long as PMU is the only
/// source this macro implements the interface directly; when a second source
/// is added, `peripherals/nmi.rs` must become the sole provider and dispatch
/// to this backend (and the new one) according to the platform strategy.
#[macro_export]
macro_rules! nmi_pmu_if_impl {
    () => {
        use kplat::nm_irq::NmiCb;

        #[kiface::provide]
        impl kplat::nm_irq::PeriodicNmiSourceIf {
            fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool {
                // All steps, including rollback, belong to the same CPU.
                // The helpers' individual guards do not cover migration
                // between counter setup, callback publication and PPI setup.
                let _guard = kspin::NoPreemptIrqSave::new();
                if !$crate::peripherals::pmu::lifecycle::can_start() {
                    return false;
                }
                // ── 1. Compute cycle threshold ────────────────────────
                // TODO: read CPU max frequency from DT OPP table (opp-hz).
                let cpu_freq_hz: u64 = 2_500_000_000;
                let cycles = (period_ns as u128 * cpu_freq_hz as u128 / 1_000_000_000u128) as u64;

                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);

                // ── 2. Fallible per‑CPU setup, counter first ──────────
                // Create the counter and register its handler before the
                // line is promoted, so a failure at most leaves counter
                // state that `deinit_cycle_counter()` rolls back.  The
                // counter is not started until step 4, so nothing can
                // overflow while this ordering holds.
                if !$crate::peripherals::pmu::init_cycle_counter(cycles) {
                    warn!("enable_periodic_nmi: cycle counter already initialised");
                    return false;
                }

                if !$crate::peripherals::pmu::register_overflow_handler(
                    $crate::peripherals::pmu::CYCLE_COUNTER_IDX,
                    handler,
                ) {
                    warn!(
                        "enable_periodic_nmi: failed to register overflow handler for cycle \
                         counter {}",
                        $crate::peripherals::pmu::CYCLE_COUNTER_IDX,
                    );
                    $crate::peripherals::pmu::deinit_cycle_counter();
                    return false;
                }

                // ── 3. Promote this CPU's PMU line to NMI delivery ────
                // The PMU interrupt is a PPI, so its priority lives in the
                // local redistributor: every CPU must configure its own
                // line.  The overflow‑dispatch handler on this line is
                // registered by the `pmu` feature init, so this path only
                // changes how that same line is delivered.
                if !khal::irq::configure_nmi(irq) {
                    // NmiMechanismIf::configure_nmi logged the reason; roll back the
                    // counter state so a retry starts from a clean CPU.
                    $crate::peripherals::pmu::deinit_cycle_counter();
                    return false;
                }

                if !$crate::peripherals::pmu::enable_periodic_irq(kirq::gic_level_irq_desc(irq)) {
                    $crate::peripherals::pmu::deinit_cycle_counter();
                    return false;
                }

                // ── 4. Start the counter ─────────────────────────────
                $crate::peripherals::pmu::enable($crate::peripherals::pmu::CYCLE_COUNTER_IDX);

                true
            }

            fn quiesce_periodic_nmi() {
                // Keep the counter and callback allocated: this is a
                // one-way terminal path, and a pending PMU interrupt will
                // observe PmuCounter's disabled atomic state and skip the
                // watchdog callback.
                $crate::peripherals::pmu::disable($crate::peripherals::pmu::CYCLE_COUNTER_IDX);
            }

            fn shutdown_periodic_nmi() -> bool {
                $crate::peripherals::pmu::shutdown_periodic_source()
            }
        }
    };
}
