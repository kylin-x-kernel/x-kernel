// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::btree_map::BTreeMap;
use core::{
    alloc::Layout,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use backtrace::Backtrace;
use kcpu_slot::cpu_slot_cell;
use kspin::{NoPreemptIrqSave, SpinNoIrq, static_lock};

pub(crate) static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

cpu_slot_cell! {
    pub(crate) static IN_GLOBAL_ALLOCATOR: AtomicBool = AtomicBool::new(false);
}

/// Pins diagnostic tracking to one CPU and restores its recursion marker.
///
/// Tracking deliberately holds IRQs and preemption disabled across the
/// callback. It is a diagnostic-only feature, and this cost avoids migration
/// or interrupt recursion while allocation itself may recursively allocate.
struct TrackingRecursionGuard {
    pin: NoPreemptIrqSave,
}

impl TrackingRecursionGuard {
    fn try_enter() -> Option<Self> {
        let pin = NoPreemptIrqSave::new();
        // The guard fixes the current CPU for the returned reference; the slot
        // contains an atomic, so shared access is valid across readers.
        let marker = IN_GLOBAL_ALLOCATOR.get(&pin);
        (!marker.swap(true, Ordering::AcqRel)).then_some(Self { pin })
    }
}

impl Drop for TrackingRecursionGuard {
    fn drop(&mut self) {
        // `self.pin` still fixes this destructor to the CPU whose marker was
        // set, and the atomic permits shared concurrent access.
        let marker = IN_GLOBAL_ALLOCATOR.get(&self.pin);
        marker.store(false, Ordering::Release);
    }
}

/// Metadata for each allocation made by the global allocator.
#[derive(Debug)]
pub struct AllocationInfo {
    /// Layout of the allocation.
    pub layout: Layout,
    /// Backtrace at the time of allocation.
    pub backtrace: Backtrace,
    /// Generation at which the allocation was made.
    pub generation: u64,
}

pub(crate) struct GlobalState {
    // FIXME: don't know why using HashMap causes crash
    pub map: BTreeMap<usize, AllocationInfo>,
    pub generation: u64,
}

static_lock! {
    static STATE: SpinNoIrq<GlobalState> = SpinNoIrq::new(GlobalState {
        map: BTreeMap::new(),
        generation: 0,
    });
}

/// Enables allocation tracking.
pub fn enable_tracking() {
    TRACKING_ENABLED.store(true, Ordering::SeqCst);
}

/// Disables allocation tracking.
pub fn disable_tracking() {
    TRACKING_ENABLED.store(false, Ordering::SeqCst);
}

/// Returns whether allocation tracking is enabled.
pub fn tracking_enabled() -> bool {
    TRACKING_ENABLED.load(Ordering::SeqCst)
}

pub(crate) fn with_state<R>(f: impl FnOnce(Option<&mut GlobalState>) -> R) -> R {
    if !tracking_enabled() {
        return f(None);
    }
    let Some(_recursion_guard) = TrackingRecursionGuard::try_enter() else {
        return f(None);
    };
    let mut state = STATE.lock();
    f(Some(&mut state))
}

/// Returns the current generation of the global allocator.
///
/// The generation is incremented every time a new allocation is made. It
/// can be utilized to track the changes in the allocation state over time.
///
/// See [`allocations_in`].
pub fn current_generation() -> u64 {
    STATE.lock().generation
}

/// Visits all allocations made by the global allocator within the given
/// generation range.
pub fn allocations_in(range: Range<u64>, visitor: impl FnMut(&AllocationInfo)) {
    with_state(|state| {
        state
            .unwrap()
            .map
            .values()
            .filter(move |info| range.contains(&info.generation))
            .for_each(visitor)
    });
}

#[unittest::mod_test]
pub mod tests_tracking {
    use unittest::def_test;

    use super::{allocations_in, disable_tracking, enable_tracking, tracking_enabled};

    #[def_test]
    fn test_tracking_toggle() {
        disable_tracking();
        assert!(!tracking_enabled());
        enable_tracking();
        assert!(tracking_enabled());
        disable_tracking();
        assert!(!tracking_enabled());
    }

    #[def_test]
    fn test_allocations_in_empty() {
        enable_tracking();
        let mut count = 0usize;
        allocations_in(0..0, |_| count += 1);
        assert_eq!(count, 0);
        disable_tracking();
    }
}
