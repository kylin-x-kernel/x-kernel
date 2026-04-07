// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform monotonic timer interface and helpers.

use core::time::Duration;

use kplat_macros::device_interface;

/// Monotonic time representation.
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
pub trait PlatMonotonicTimer {
    /// Returns the current monotonic timer tick count.
    fn now_ticks() -> u64;
    /// Converts monotonic timer ticks to nanoseconds.
    fn t2ns(t: u64) -> u64;
    /// Returns the monotonic timer frequency in Hz.
    fn freq() -> u64; // Hz
    /// Converts monotonic nanoseconds to timer ticks.
    fn ns2t(ns: u64) -> u64;

    /// Returns the monotonic timer interrupt ID.
    fn interrupt_id() -> usize;

    /// Arms the monotonic timer to trigger at the given deadline (in ns).
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

/// Busy-waits for the given duration.
pub fn spin_wait(d: Duration) {
    spin_until(now() + d);
}

/// Busy-waits until the given deadline.
pub fn spin_until(dl: ClockTime) {
    while now() < dl {
        core::hint::spin_loop();
    }
}
