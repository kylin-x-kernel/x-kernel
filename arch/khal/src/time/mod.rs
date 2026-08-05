// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified time capability exposed by `khal`.

use ktime_types::{MonotonicInstant, TimeSpan};

/// Raw counter ticks in the platform monotonic timer domain.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimerTicks(u64);

impl TimerTicks {
    /// Wraps a raw hardware counter value at a timer backend boundary.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Exposes the raw counter representation at a hardware or ABI boundary.
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Returns the wrapping tick delta between two counter samples.
    pub const fn wrapping_duration_since(self, earlier: Self) -> Self {
        Self(self.0.wrapping_sub(earlier.0))
    }
}

#[kiface::interface]
pub trait MonotonicTimerIf {
    /// Returns the current monotonic timer tick count.
    fn now_ticks() -> TimerTicks;
    /// Converts monotonic timer ticks to elapsed time.
    fn ticks_to_span(ticks: TimerTicks) -> TimeSpan;
    /// Returns the monotonic timer frequency in Hz.
    fn freq() -> u64;
    /// Converts elapsed time to monotonic timer ticks.
    fn span_to_ticks(span: TimeSpan) -> TimerTicks;
    /// Returns the monotonic timer interrupt ID.
    fn interrupt_id() -> usize;
    /// Arms the monotonic timer to trigger at the given deadline.
    fn arm_timer(deadline: MonotonicInstant);
    /// Allows the timer backend to handle counter/timer repair after idle returns.
    fn handle_idle_return(previous_ticks: TimerTicks) -> bool;
}

#[inline]
pub fn now_ticks() -> TimerTicks {
    MonotonicTimerIf::now_ticks()
}

#[inline]
pub fn ticks_to_span(ticks: TimerTicks) -> TimeSpan {
    MonotonicTimerIf::ticks_to_span(ticks)
}

#[inline]
pub fn freq() -> u64 {
    MonotonicTimerIf::freq()
}

#[inline]
pub fn span_to_ticks(span: TimeSpan) -> TimerTicks {
    MonotonicTimerIf::span_to_ticks(span)
}

#[inline]
pub fn interrupt_id() -> usize {
    MonotonicTimerIf::interrupt_id()
}

#[inline]
pub fn arm_timer(deadline: MonotonicInstant) {
    MonotonicTimerIf::arm_timer(deadline)
}

#[inline]
pub fn handle_idle_return(previous_ticks: TimerTicks) -> bool {
    MonotonicTimerIf::handle_idle_return(previous_ticks)
}

#[inline]
pub fn monotonic_time() -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(ticks_to_span(now_ticks()))
}

#[inline]
pub fn spin_until(deadline: MonotonicInstant) {
    while monotonic_time() < deadline {
        core::hint::spin_loop();
    }
}

#[inline]
pub fn spin_wait(dur: TimeSpan) {
    spin_until(monotonic_time() + dur);
}

/// Busy-wait for the given duration.
pub fn busy_wait(dur: TimeSpan) {
    spin_wait(dur);
}

/// Busy-wait until the given deadline.
pub fn busy_wait_until(deadline: MonotonicInstant) {
    spin_until(deadline);
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_time {
    use ktime_types::{NANOS_PER_SEC, TimeSpan};
    use unittest::def_test;

    #[def_test]
    fn test_duration_from_nanos() {
        let nanos = NANOS_PER_SEC;
        let from = TimeSpan::from_nanos(nanos);
        let one = TimeSpan::from_secs(1);
        assert_eq!(from, one);
    }

    #[def_test]
    fn test_duration_ordering() {
        let short = TimeSpan::from_millis(1);
        let long = TimeSpan::from_millis(2);
        assert!(long > short);
    }
}
