// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process CPU-time clock ticks.

use ktime_types::{NANOS_PER_SEC, TimeSpan};

/// `USER_HZ`: the fixed rate at which `clock_t` values are exposed to
/// userspace via `times(2)`, `SIGCHLD` (`si_utime`/`si_stime`), and
/// `signalfd`.
///
/// This is a userspace ABI constant (Linux `include/uapi/asm-generic/param.h`
/// pins it to 100) and is **deliberately decoupled** from the kernel scheduler
/// tick rate (`kbuild_config::TICKS_PER_SECOND`, the analogue of Linux
/// `CONFIG_HZ`). Linux keeps the two separate and converts with
/// `jiffies_to_clock_t()`; tying the user-visible `clock_t` to the scheduler
/// tick here would silently change the `times(2)`/`SIGCHLD`/`signalfd` ABI
/// whenever the scheduler frequency is reconfigured.
pub const USER_HZ: u64 = 100;

const CLOCK_TICKS_PER_SECOND: u64 = USER_HZ;

/// A process CPU-time value in the POSIX `clock_t` tick domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PosixClockTicks(u64);

impl PosixClockTicks {
    /// Converts a semantic time span to POSIX clock ticks.
    ///
    /// Fractional ticks are truncated as required by the integer `clock_t`
    /// ABI. Values exceeding the raw ABI carrier saturate at `u64::MAX`.
    pub const fn from_time_span(span: TimeSpan) -> Self {
        let fractional_ticks = span.subsec_nanos() as u64 * CLOCK_TICKS_PER_SECOND / NANOS_PER_SEC;
        let ticks = match span.as_secs().checked_mul(CLOCK_TICKS_PER_SECOND) {
            Some(whole_ticks) => whole_ticks.saturating_add(fractional_ticks),
            None => u64::MAX,
        };
        Self(ticks)
    }

    /// Constructs POSIX clock ticks from an ABI carrier.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value for an ABI boundary.
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Converts POSIX clock ticks to a semantic time span.
    pub const fn to_time_span(self) -> TimeSpan {
        let seconds = self.0 / CLOCK_TICKS_PER_SECOND;
        let fractional_ticks = self.0 % CLOCK_TICKS_PER_SECOND;
        let nanoseconds = fractional_ticks * NANOS_PER_SEC / CLOCK_TICKS_PER_SECOND;
        TimeSpan::new(seconds, nanoseconds as u32)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_clock_ticks_round_trip_exact_values() {
        let span = TimeSpan::from_nanos(NANOS_PER_SEC / CLOCK_TICKS_PER_SECOND * 2);
        let ticks = PosixClockTicks::from_time_span(span);
        assert_eq!(ticks.as_raw(), 2);
        assert_eq!(ticks.to_time_span(), span);
    }

    #[def_test]
    fn test_clock_ticks_truncate_sub_tick_duration() {
        assert_eq!(
            PosixClockTicks::from_time_span(TimeSpan::from_nanos(1)).as_raw(),
            0
        );
    }

    #[def_test]
    fn test_clock_ticks_saturate_large_duration() {
        assert_eq!(
            PosixClockTicks::from_time_span(TimeSpan::MAX).as_raw(),
            u64::MAX
        );
    }
}
