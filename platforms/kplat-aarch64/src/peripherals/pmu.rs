// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use aarch64_pmuv3::pmuv3::{PmuCounter, PmuEvent};
use kplat::perf::PerfCb;
use lazyinit::LazyInit;
const MAX_PMU_COUNTERS: usize = 32;

/// Index of the PMU cycle counter (PMCCNTR), used by the periodic NMI backend.
pub const CYCLE_COUNTER_IDX: u32 = (MAX_PMU_COUNTERS - 1) as u32;

pub struct PmuManager {
    counters: [Option<PmuCounter>; MAX_PMU_COUNTERS],
    overflow_handlers: [Option<PerfCb>; MAX_PMU_COUNTERS],
}
#[percpu::def_percpu]
static PMU: LazyInit<PmuManager> = LazyInit::new();

/// Returns a mutable reference to the current CPU's [`PmuManager`],
/// initialising it on first access.
///
/// # Safety
///
/// Caller must run on the CPU whose PMU manager is being accessed, with
/// preemption and interrupts disabled to prevent migration to a different
/// CPU during the call.
#[inline]
unsafe fn ensure_pmu_inited() -> &'static mut PmuManager {
    // SAFETY: callers run in per‑CPU init or with preemption disabled,
    // so `current_ref_mut_raw` accesses the caller's own CPU instance.
    let pmu = unsafe { PMU.current_ref_mut_raw() };
    pmu.call_once(|| PmuManager {
        counters: [const { None }; MAX_PMU_COUNTERS],
        overflow_handlers: [const { None }; MAX_PMU_COUNTERS],
    });
    pmu
}
pub fn reg_handler_overflow_handler(index: u32, handler: PerfCb) -> bool {
    let idx = index as usize;
    if idx >= MAX_PMU_COUNTERS {
        warn!("reg_handler_overflow_handler: index {idx} out of range");
        return false;
    }
    // SAFETY: per‑CPU access on the current CPU; NMI/IRQ context prevents
    // migration.
    unsafe {
        let pmu = PMU.current_ref_mut_raw();
        if pmu.counters[idx].is_none() {
            warn!("reg_handler_overflow_handler: counter {idx} not initialised");
            return false;
        }
        pmu.overflow_handlers[idx] = Some(handler);
        true
    }
}
pub fn init_cycle_counter(threshold: u64) -> bool {
    // SAFETY: `ensure_pmu_inited` and per‑CPU counter management run
    // during early init on the owning CPU; no concurrent access.
    unsafe {
        let pmu_mgr = ensure_pmu_inited();
        let idx = MAX_PMU_COUNTERS - 1;
        if pmu_mgr.counters[idx].is_some() {
            return false;
        }
        let counter = PmuCounter::new_cycle_counter(threshold);
        if counter.check_pmu_support().is_err() {
            return false;
        }
        pmu_mgr.counters[idx] = Some(counter);
        true
    }
}
/// Release this CPU's cycle counter and its overflow handler.
///
/// Rolls back a partially failed [`enable_periodic_nmi`]: the counter is
/// disabled (if it was ever started) and both the counter and handler slots
/// are cleared so a later arming attempt starts from a clean per‑CPU state.
pub fn deinit_cycle_counter() {
    // SAFETY: per‑CPU access; callers run in init context on the owning CPU.
    unsafe {
        let pmu = PMU.current_ref_mut_raw();
        if let Some(Some(counter)) = pmu.counters.get_mut(CYCLE_COUNTER_IDX as usize) {
            counter.disable();
        }
        pmu.counters[CYCLE_COUNTER_IDX as usize] = None;
        pmu.overflow_handlers[CYCLE_COUNTER_IDX as usize] = None;
    }
}
pub fn init_event_counter(index: u32, threshold: u64, event: PmuEvent) -> bool {
    let idx = index as usize;
    if idx >= MAX_PMU_COUNTERS - 1 {
        return false;
    }
    // SAFETY: per‑CPU init on the owning CPU; no concurrent access.
    unsafe {
        let pmu_mgr = ensure_pmu_inited();
        if pmu_mgr.counters[idx].is_some() {
            return false;
        }
        let counter = PmuCounter::new_event_counter(index, threshold, event);
        if counter.check_pmu_support().is_err() {
            return false;
        }
        pmu_mgr.counters[idx] = Some(counter);
        true
    }
}
/// Run a closure with a mutable reference to the PMU counter at `index`
/// on the current CPU.
///
/// # Safety
///
/// Caller must run with preemption or interrupts disabled on the current
/// CPU to prevent migration.  `index` must be a valid counter index that
/// has been previously initialised via [`init_cycle_counter`] or
/// [`init_event_counter`]; accessing an uninitialised counter is a no‑op
/// (the closure is silently skipped).
#[inline]
unsafe fn with_counter_mut<F>(index: u32, f: F)
where
    F: FnOnce(&mut PmuCounter),
{
    if let Some(Some(counter)) =
        // SAFETY: per‑CPU access on the current CPU; caller ensures
        // preemption / migration is disabled.
        unsafe { PMU.current_ref_mut_raw().counters.get_mut(index as usize) }
    {
        f(counter);
    }
}

/// Run a closure with a shared reference to the PMU counter at `index` on
/// the current CPU.
///
/// # Safety
///
/// Caller must be pinned to the current CPU. Counter slots are only changed
/// during per-CPU setup or rollback before a source is enabled; once armed,
/// operations exposed through this helper only use [`PmuCounter`] methods
/// that take `&self`, so they may race safely with PMU IRQ/NMI dispatch.
#[inline]
unsafe fn with_counter<F>(index: u32, f: F)
where
    F: FnOnce(&PmuCounter),
{
    // SAFETY: caller is pinned to the current CPU, so this only accesses its
    // per-CPU PMU state. `get` also makes a terminal quiesce before PMU setup
    // a harmless no-op.
    let pmu = unsafe { PMU.current_ref_raw() };
    let Some(pmu) = pmu.get() else {
        return;
    };
    if let Some(Some(counter)) = pmu.counters.get(index as usize) {
        f(counter);
    }
}
pub fn enable(index: u32) {
    // SAFETY: callers run on the owning CPU with preemption or IRQs disabled.
    // `PmuCounter::enable` only mutates its atomic enabled state and PMU
    // registers, so shared access remains valid if an NMI observes it.
    unsafe {
        with_counter(index, |c| c.enable());
    }
}
pub fn disable(index: u32) {
    // SAFETY: see `enable`. This is also used by the terminal NMI quiesce
    // path, where an in-flight NMI may concurrently read the counter state.
    unsafe {
        with_counter(index, |c| c.disable());
    }
}
pub fn is_enabled(index: u32) -> bool {
    let mut enabled = false;
    // SAFETY: read-only per-CPU access on the current CPU.
    unsafe { with_counter(index, |counter| enabled = counter.is_enabled()) };
    enabled
}
pub fn dispatch_irq_overflows() -> bool {
    // SAFETY: called from PMU IRQ/NMI context on the current CPU. Setup or
    // rollback never overlaps an armed source, and active counters expose
    // their state through `&self` plus atomics, so shared access also permits
    // the terminal quiesce path to race with a pending overflow safely.
    let pmu = unsafe { PMU.current_ref_raw() };
    let Some(pmu) = pmu.get() else {
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
    // SAFETY: see `enable`.
    unsafe {
        with_counter_mut(index, |c| c.set_threshold(threshold));
    }
}
#[macro_export]
macro_rules! pmu_if_impl {
    () => {
        use kplat::perf::PerfCb;

        #[impl_dev_interface]
        impl kplat::perf::PerfMgr {
            fn on_overflow() -> bool {
                $crate::peripherals::pmu::dispatch_irq_overflows()
            }

            fn reg_cb(index: u32, handler: PerfCb) -> bool {
                $crate::peripherals::pmu::reg_handler_overflow_handler(index, handler)
            }

            fn register_overflow_irq() -> bool {
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                let desc = kirq::gic_level_irq_desc(irq);
                let handler = alloc::sync::Arc::new(|_| {
                    $crate::peripherals::pmu::dispatch_irq_overflows();
                    kirq::IrqEvent::HANDLED
                });
                // Normal IRQ delivery in pmu-only builds; NMI registration
                // (NMI table + normal-IRQ fallback) when the PMU is the
                // compiled NMI source, so the line can later be promoted.
                #[cfg(feature = "nmi-pmu")]
                {
                    kirq::register_nmi(desc, handler)
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    kirq::register(desc, handler)
                }
            }

            fn enable_irq() {
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                // Use the same descriptor as the registration so the enable
                // targets the same resolved line (bare numbers would fall
                // back to the PlainVirq identity path).
                kirq::enable(kirq::gic_level_irq_desc(irq), true);
            }
        }
    };
}

/// Implements the source‑neutral [`kplat::nm_irq::NmiPeriodic`] interface
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
/// `NmiPeriodic` allows exactly one provider.  As long as PMU is the only
/// source this macro implements the interface directly; when a second source
/// is added, `peripherals/nmi.rs` must become the sole provider and dispatch
/// to this backend (and the new one) according to the platform strategy.
#[macro_export]
macro_rules! nmi_pmu_if_impl {
    () => {
        use kplat::nm_irq::NmiCb;

        #[impl_dev_interface]
        impl kplat::nm_irq::NmiPeriodic {
            fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool {
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

                if !$crate::peripherals::pmu::reg_handler_overflow_handler(
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
                    // NmiDef::configure_nmi logged the reason; roll back the
                    // counter state so a retry starts from a clean CPU.
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
        }
    };
}
