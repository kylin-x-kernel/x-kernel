// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Soft/hard lockup detection state and helpers.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::watchdog_task::WatchdogTask;

/// Default soft-lockup detection threshold.
pub const DEFAULT_SOFTLOCKUP_THRESHOLD: TimeSpan = TimeSpan::from_secs(20);

/// Default hard-lockup detection threshold.
pub const DEFAULT_HARDLOCKUP_THRESHOLD: TimeSpan = TimeSpan::from_secs(10);

/// Independent watchdog sample period (Linux `sample_period`).
///
/// Softlockup threshold / 5 so the oneshot hrtimer has several heartbeats
/// before a hardlockup NMI window. With a 20s soft threshold this is 4s.
pub const DEFAULT_WATCHDOG_SAMPLE_PERIOD: TimeSpan = TimeSpan::from_secs(4);

/// Per-CPU lockup detection state.
#[repr(C, align(64))]
pub struct LockupDetection {
    // === Softlockup Detection ===
    /// Timestamp when watchdog thread last ran (nanoseconds).
    /// Updated by watchdog thread, checked by timer interrupt.
    soft_timestamp: AtomicU64,
    soft_timestamp_initialized: AtomicBool,

    // === Hardlockup Detection ===
    /// Timestamp of the last hardlockup heartbeat (nanoseconds).
    ///
    /// Written on every local timer IRQ and by the 4s watchdog periodic
    /// callback; read from NMI. Compared against wall time so the check
    /// does not depend on the PMU NMI interval.
    hard_timestamp: AtomicU64,
    hard_timestamp_initialized: AtomicBool,
}

impl Default for LockupDetection {
    fn default() -> Self {
        Self::new()
    }
}

impl LockupDetection {
    /// Create a new LockupDetection instance.
    pub const fn new() -> Self {
        Self {
            soft_timestamp: AtomicU64::new(0),
            soft_timestamp_initialized: AtomicBool::new(false),
            hard_timestamp: AtomicU64::new(0),
            hard_timestamp_initialized: AtomicBool::new(false),
        }
    }

    // =========================================================================
    // Softlockup detection
    // =========================================================================

    /// Update the soft timestamp (called by watchdog thread).
    ///
    /// The watchdog thread should call this every time it gets scheduled.
    #[inline]
    pub fn touch_softlockup(&self, timestamp: MonotonicInstant) {
        self.soft_timestamp
            .store(timestamp.as_nanos_u64_saturating(), Ordering::Relaxed);
        self.soft_timestamp_initialized
            .store(true, Ordering::Release);
    }

    /// Get the soft timestamp.
    #[inline]
    pub fn soft_timestamp(&self) -> Option<MonotonicInstant> {
        if !self.soft_timestamp_initialized.load(Ordering::Acquire) {
            return None;
        }
        Some(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(self.soft_timestamp.load(Ordering::Relaxed)),
        ))
    }

    /// Check for softlockup condition.
    ///
    /// Call this from timer interrupt context.
    /// Returns true if softlockup is detected.
    #[inline]
    pub fn check_softlockup(&self, now: MonotonicInstant, threshold: TimeSpan) -> bool {
        let Some(last) = self.soft_timestamp() else {
            return false;
        };
        now.saturating_duration_since(last) > threshold
    }

    // =========================================================================
    // Hardlockup detection
    // =========================================================================

    /// Record a watchdog sample (timer IRQ or the 4s periodic callback).
    #[inline]
    pub fn timer_tick(&self, now: MonotonicInstant) {
        self.hard_timestamp
            .store(now.as_nanos_u64_saturating(), Ordering::Relaxed);
        self.hard_timestamp_initialized
            .store(true, Ordering::Release);
    }

    /// Check for hardlockup condition (called from NMI).
    ///
    /// Returns true when no watchdog sample has landed for longer than
    /// `threshold`. Wall time is required because the NMI source is a PMU
    /// cycle budget at an assumed CPU frequency: two NMIs can fall between
    /// 4s samples on a live CPU.
    #[inline]
    pub fn check_hardlockup(&self, now: MonotonicInstant, threshold: TimeSpan) -> bool {
        if !self.hard_timestamp_initialized.load(Ordering::Acquire) {
            return false;
        }
        let last = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(
            self.hard_timestamp.load(Ordering::Relaxed),
        ));
        now.saturating_duration_since(last) > threshold
    }
}

#[percpu::def_percpu]
pub static LOCKUP_DETECTION: LockupDetection = LockupDetection::new();

/// Touch softlockup timestamp (called from watchdog thread).
#[inline]
pub fn touch_softlockup(timestamp: MonotonicInstant) {
    // SAFETY: `current_ref_mut_raw` accesses the per‑CPU instance for the
    // current CPU.  The watchdog thread is pinned to its CPU and runs with
    // preemption disabled, so the pointer cannot race with migration.
    unsafe {
        LOCKUP_DETECTION
            .current_ref_mut_raw()
            .touch_softlockup(timestamp);
    }
}

/// Refresh the hardlockup heartbeat.
///
/// Called from every local timer IRQ and from the 4s watchdog periodic
/// callback. The IRQ path is the source of truth: a live CPU that still
/// takes timer interrupts will keep this timestamp fresh even if the 4s
/// sample is delayed.
#[inline]
pub fn timer_tick(now: MonotonicInstant) {
    // SAFETY: `current_ref_mut_raw` accesses the per‑CPU instance for the
    // current CPU.  Timer callbacks run with interrupts disabled on the
    // local CPU, so the pointer is stable.
    unsafe {
        LOCKUP_DETECTION.current_ref_mut_raw().timer_tick(now);
    }
}

/// Check softlockup of a CPU.
#[inline]
pub fn check_softlockup(now: MonotonicInstant) -> bool {
    // SAFETY: `current_ref_mut_raw` accesses the per‑CPU instance for the
    // current CPU from timer interrupt context (interrupts disabled).
    unsafe {
        LOCKUP_DETECTION
            .current_ref_mut_raw()
            .check_softlockup(now, DEFAULT_SOFTLOCKUP_THRESHOLD)
    }
}

/// Register the hard lockup detection task on the current CPU.
pub fn register_hardlockup_detection_task() {
    // SAFETY: `current_ref_raw` obtains a shared reference to the per‑CPU
    // `LockupDetection` for the current CPU.  The returned `&'static`
    // reference is valid as long as the per‑CPU storage exists (the
    // lifetime of the kernel).  Called once during init on the owning CPU.
    let task: &'static LockupDetection = unsafe { LOCKUP_DETECTION.current_ref_raw() };
    crate::watchdog_task::register_watchdog_task(task);
}

impl WatchdogTask for LockupDetection {
    fn name(&self) -> &str {
        "HardLockupDetection"
    }

    fn check(&self) -> bool {
        !self.check_hardlockup(khal::time::monotonic_time(), DEFAULT_HARDLOCKUP_THRESHOLD)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    fn instant_secs(secs: u64) -> MonotonicInstant {
        MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(secs))
    }

    #[def_test]
    fn hardlockup_uses_wall_time_not_nmi_spacing() {
        let det = LockupDetection::new();
        let t0 = instant_secs(1);
        assert!(!det.check_hardlockup(t0, DEFAULT_HARDLOCKUP_THRESHOLD));

        det.timer_tick(t0);
        assert!(!det.check_hardlockup(instant_secs(5), DEFAULT_HARDLOCKUP_THRESHOLD));
        assert!(!det.check_hardlockup(instant_secs(11), DEFAULT_HARDLOCKUP_THRESHOLD));
        assert!(det.check_hardlockup(instant_secs(12), DEFAULT_HARDLOCKUP_THRESHOLD));
    }
}
