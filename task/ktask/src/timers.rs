// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer tick callbacks and time-based event dispatch.

use alloc::{boxed::Box, vec::Vec};

use kspin::NoPreemptIrqSave;
use ktime_types::SystemTime;

percpu_static! {
    TIMER_CALLBACKS: Vec<Box<dyn Fn(SystemTime) + Send + Sync>> = Vec::new(),
}

/// Registers a callback function to be called on each timer tick.
pub fn register_timer_callback<F>(callback: F)
where
    F: Fn(SystemTime) + Send + Sync + 'static,
{
    let _g = NoPreemptIrqSave::new();
    // SAFETY: the per-CPU callback vector is accessed on the current CPU while
    // preemption and interrupts are masked by `NoPreemptIrqSave`.
    unsafe {
        TIMER_CALLBACKS
            .current_ref_mut_raw()
            .push(Box::new(callback))
    };
}

pub(crate) fn check_events() {
    // SAFETY: iterates the current CPU's callback vector while running in the
    // timer path that owns this per-CPU storage.
    for callback in unsafe { TIMER_CALLBACKS.current_ref_raw().iter() } {
        callback(ktime::realtime());
    }
    // The timer-wheel drain is done separately in `on_timer_fire` (merged with
    // rearm into a single wheel-lock acquisition) to avoid taking the lock twice
    // on every timer IRQ.
}
