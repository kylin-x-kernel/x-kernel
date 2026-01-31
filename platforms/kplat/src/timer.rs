// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform timer interface and helpers.

use core::time::Duration;

use kplat_macros::device_interface;

/// Wall-clock time representation.
pub type ClockTime = Duration;

/// Milliseconds per second.
pub const MS_SEC: u64 = 1_000;
/// Microseconds per second.
pub const US_SEC: u64 = 1_000_000;
/// Nanoseconds per second.
pub const NS_SEC: u64 = 1_000_000_000;
/// Nanoseconds per millisecond.
pub const NS_MS: u64 = 1_000_000;
/// Nanoseconds per microsecond.
pub const NS_US: u64 = 1_000;

#[device_interface]
pub trait GlobalTimer {
    /// Returns the current timer tick count.
    fn now_ticks() -> u64;
    /// Converts ticks to nanoseconds.
    fn t2ns(t: u64) -> u64;
    /// Returns timer frequency in Hz.
    fn freq() -> u64; // Hz
    /// Converts nanoseconds to ticks.
    fn ns2t(ns: u64) -> u64;
    /// Returns wall clock offset in nanoseconds.
    fn offset_ns() -> u64;

    /// Returns the timer interrupt ID.
    fn interrupt_id() -> usize;

    /// Arms the timer to trigger at the given deadline (in ns).
    fn arm_timer(deadline: u64);
}

/// Returns the current monotonic time in nanoseconds.
pub fn now_ns() -> u64 {
    t2ns(now_ticks())
}

/// Returns the current monotonic time as `ClockTime`.
pub fn now() -> ClockTime {
    ClockTime::from_nanos(now_ns())
}

/// Returns the wall-clock time in nanoseconds.
pub fn wall_ns() -> u64 {
    now_ns() + offset_ns()
}

/// Returns the wall-clock time as `ClockTime`.
pub fn wall() -> ClockTime {
    ClockTime::from_nanos(wall_ns())
}

/// Busy-waits for the given duration.
pub fn spin_wait(d: Duration) {
    spin_until(wall() + d);
}

/// Busy-waits until the given deadline.
pub fn spin_until(dl: ClockTime) {
    while wall() < dl {
        core::hint::spin_loop();
    }
}
