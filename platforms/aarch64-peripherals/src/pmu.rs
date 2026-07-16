// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use aarch64_pmuv3::pmuv3::{PmuCounter, PmuEvent};
use kplat::perf::PerfCb;
use lazyinit::LazyInit;
const MAX_PMU_COUNTERS: usize = 32;
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
        return false;
    }
    // SAFETY: per‑CPU access on the current CPU; NMI/IRQ context prevents
    // migration.
    unsafe {
        let pmu = PMU.current_ref_mut_raw();
        if pmu.counters[idx].is_none() {
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
pub fn enable(index: u32) {
    // SAFETY: `with_counter_mut` accesses the current CPU's PMU state;
    // callers run with preemption or IRQs disabled.
    unsafe {
        with_counter_mut(index, |c| c.enable());
    }
}
pub fn disable(index: u32) {
    // SAFETY: see `enable`.
    unsafe {
        with_counter_mut(index, |c| c.disable());
    }
}
pub fn is_enabled(index: u32) -> bool {
    // SAFETY: read‑only per‑CPU access on the current CPU.
    unsafe {
        PMU.current_ref_mut_raw()
            .counters
            .get(index as usize)
            .and_then(|c| c.as_ref())
            .map(|c| c.is_enabled())
            .unwrap_or(false)
    }
}
pub fn dispatch_irq_overflows() -> bool {
    // SAFETY: called from PMU IRQ handler; runs with interrupts disabled
    // on the current CPU — per‑CPU access is safe.
    unsafe {
        let pmu = PMU.current_ref_mut_raw();
        let mut dispatch_irqd_any = false;
        for idx in 0..MAX_PMU_COUNTERS {
            let handler = pmu.overflow_handlers[idx];
            let Some(counter) = pmu.counters[idx].as_mut() else {
                continue;
            };
            if counter.handle_overflow().is_ok() {
                dispatch_irqd_any = true;
                if let Some(h) = handler {
                    h();
                }
            }
        }
        dispatch_irqd_any
    }
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
                $crate::pmu::dispatch_irq_overflows()
            }

            fn reg_cb(index: u32, handler: PerfCb) -> bool {
                $crate::pmu::reg_handler_overflow_handler(index, handler)
            }
        }
    };
}
