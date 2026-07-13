// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified time capability exposed by `khal`.

pub use core::time::Duration;
pub type TimeValue = Duration;

use crate::rtc;

pub const MILLIS_PER_SEC: u64 = 1_000;
pub const MICROS_PER_SEC: u64 = 1_000_000;
pub const NANOS_PER_SEC: u64 = 1_000_000_000;
pub const NANOS_PER_MILLIS: u64 = 1_000_000;
pub const NANOS_PER_MICROS: u64 = 1_000;

pub use MICROS_PER_SEC as US_SEC;
pub use MILLIS_PER_SEC as MS_SEC;
pub use NANOS_PER_MICROS as NS_US;
pub use NANOS_PER_MILLIS as NS_MS;
pub use NANOS_PER_SEC as NS_SEC;

#[kiface::interface]
pub trait MonotonicTimerIf {
    /// Returns the current monotonic timer tick count.
    fn now_ticks() -> u64;
    /// Converts monotonic timer ticks to nanoseconds.
    fn t2ns(ticks: u64) -> u64;
    /// Returns the monotonic timer frequency in Hz.
    fn freq() -> u64;
    /// Converts monotonic nanoseconds to timer ticks.
    fn ns2t(nanos: u64) -> u64;
    /// Returns the monotonic timer interrupt ID.
    fn interrupt_id() -> usize;
    /// Arms the monotonic timer to trigger at the given deadline (in ns).
    fn arm_timer(deadline: u64);
    /// Allows the timer backend to handle counter/timer repair after idle returns.
    fn handle_idle_return(previous_ticks: u64) -> bool;
}

#[inline]
pub fn now_ticks() -> u64 {
    MonotonicTimerIf::now_ticks()
}

#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    MonotonicTimerIf::t2ns(ticks)
}

#[inline]
pub fn freq() -> u64 {
    MonotonicTimerIf::freq()
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    MonotonicTimerIf::ns2t(nanos)
}

#[inline]
pub fn interrupt_id() -> usize {
    MonotonicTimerIf::interrupt_id()
}

#[inline]
pub fn arm_timer(deadline: u64) {
    MonotonicTimerIf::arm_timer(deadline)
}

#[inline]
pub fn handle_idle_return(previous_ticks: u64) -> bool {
    MonotonicTimerIf::handle_idle_return(previous_ticks)
}

#[inline]
pub fn now_ns() -> u64 {
    t2ns(now_ticks())
}

#[inline]
pub fn now() -> TimeValue {
    TimeValue::from_nanos(now_ns())
}

pub use now as monotonic_time;
pub use now_ns as monotonic_time_nanos;

#[inline]
pub fn offset_ns() -> u64 {
    rtc::offset_ns()
}

#[inline]
pub fn wall_ns() -> u64 {
    now_ns() + offset_ns()
}

#[inline]
pub fn wall() -> TimeValue {
    TimeValue::from_nanos(wall_ns())
}

pub use wall as wall_time;
pub use wall_ns as wall_time_nanos;

#[inline]
pub fn spin_until(deadline: TimeValue) {
    while now() < deadline {
        core::hint::spin_loop();
    }
}

#[inline]
pub fn spin_wait(dur: Duration) {
    spin_until(now() + dur);
}

/// Busy-wait for the given duration.
pub fn busy_wait(dur: Duration) {
    spin_wait(dur);
}

/// Busy-wait until the given deadline.
pub fn busy_wait_until(deadline: TimeValue) {
    spin_until(deadline);
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_time {
    use unittest::def_test;

    use super::{Duration, NANOS_PER_SEC};

    #[def_test]
    fn test_duration_from_nanos() {
        let nanos = NANOS_PER_SEC;
        let from = Duration::from_nanos(nanos);
        let one = Duration::from_secs(1);
        assert_eq!(from, one);
    }

    #[def_test]
    fn test_duration_ordering() {
        let short = Duration::from_millis(1);
        let long = Duration::from_millis(2);
        assert!(long > short);
    }
}
