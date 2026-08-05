// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    fmt,
    ops::{Add, Sub},
};

use crate::{NANOS_PER_SEC, TimeSpan};

/// A signed wall-clock timestamp relative to the Unix epoch.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SystemTime {
    seconds: i64,
    nanoseconds: u32,
}

impl Default for SystemTime {
    fn default() -> Self {
        Self::UNIX_EPOCH
    }
}

impl SystemTime {
    /// The latest representable timestamp.
    pub const MAX: Self = Self {
        seconds: i64::MAX,
        nanoseconds: NANOS_PER_SEC as u32 - 1,
    };
    /// The earliest representable timestamp.
    pub const MIN: Self = Self {
        seconds: i64::MIN,
        nanoseconds: 0,
    };
    /// The Unix epoch, 1970-01-01 00:00:00 UTC.
    pub const UNIX_EPOCH: Self = Self {
        seconds: 0,
        nanoseconds: 0,
    };

    /// Creates a timestamp from normalized Unix second and nanosecond components.
    pub const fn from_unix_parts(seconds: i64, nanoseconds: u32) -> Option<Self> {
        if nanoseconds >= NANOS_PER_SEC as u32 {
            return None;
        }
        Some(Self {
            seconds,
            nanoseconds,
        })
    }

    /// Creates a timestamp from whole Unix seconds.
    pub const fn from_unix_seconds(seconds: i64) -> Self {
        Self {
            seconds,
            nanoseconds: 0,
        }
    }

    /// Creates a timestamp from signed nanoseconds relative to the Unix epoch.
    pub const fn from_unix_nanos(nanoseconds: i128) -> Option<Self> {
        let seconds = nanoseconds.div_euclid(NANOS_PER_SEC as i128);
        if seconds < i64::MIN as i128 || seconds > i64::MAX as i128 {
            return None;
        }
        Some(Self {
            seconds: seconds as i64,
            nanoseconds: nanoseconds.rem_euclid(NANOS_PER_SEC as i128) as u32,
        })
    }

    /// Returns the whole Unix seconds, rounded toward negative infinity.
    pub const fn unix_seconds(self) -> i64 {
        self.seconds
    }

    /// Returns the normalized fractional nanosecond component.
    pub const fn subsec_nanos(self) -> u32 {
        self.nanoseconds
    }

    /// Returns the total signed nanoseconds relative to the Unix epoch.
    pub const fn unix_nanos(self) -> i128 {
        self.seconds as i128 * NANOS_PER_SEC as i128 + self.nanoseconds as i128
    }

    /// Advances this timestamp, returning `None` on overflow.
    pub const fn checked_add(self, duration: TimeSpan) -> Option<Self> {
        let nanoseconds = self.nanoseconds + duration.subsec_nanos();
        let carry = (nanoseconds >= NANOS_PER_SEC as u32) as i128;
        let seconds = self.seconds as i128 + duration.as_secs() as i128 + carry;
        if seconds > i64::MAX as i128 {
            return None;
        }
        Some(Self {
            seconds: seconds as i64,
            nanoseconds: nanoseconds % NANOS_PER_SEC as u32,
        })
    }

    /// Moves this timestamp backwards, returning `None` on overflow.
    pub const fn checked_sub(self, duration: TimeSpan) -> Option<Self> {
        let duration_nanoseconds = duration.subsec_nanos();
        let (nanoseconds, borrow) = if self.nanoseconds >= duration_nanoseconds {
            (self.nanoseconds - duration_nanoseconds, 0i128)
        } else {
            (
                self.nanoseconds + NANOS_PER_SEC as u32 - duration_nanoseconds,
                1i128,
            )
        };
        let seconds = self.seconds as i128 - duration.as_secs() as i128 - borrow;
        if seconds < i64::MIN as i128 {
            return None;
        }
        Some(Self {
            seconds: seconds as i64,
            nanoseconds,
        })
    }

    /// Returns the non-negative duration since `earlier`.
    pub const fn duration_since(self, earlier: Self) -> Result<TimeSpan, SystemTimeError> {
        let delta = self.unix_nanos() - earlier.unix_nanos();
        if delta < 0 {
            return Err(SystemTimeError {
                duration: TimeSpan::try_from_nanos((-delta) as u128)
                    .expect("SystemTime difference fits in TimeSpan"),
            });
        }
        Ok(
            TimeSpan::try_from_nanos(delta as u128)
                .expect("SystemTime difference fits in TimeSpan"),
        )
    }
}

impl fmt::Debug for SystemTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemTime")
            .field("seconds", &self.seconds)
            .field("nanoseconds", &self.nanoseconds)
            .finish()
    }
}

impl Add<TimeSpan> for SystemTime {
    type Output = Self;

    /// Advances the timestamp by `rhs`.
    ///
    /// # Panics
    ///
    /// Panics if the result is outside the representable Unix timestamp range.
    fn add(self, rhs: TimeSpan) -> Self::Output {
        self.checked_add(rhs)
            .expect("system time addition overflow")
    }
}

impl Sub<TimeSpan> for SystemTime {
    type Output = Self;

    /// Moves the timestamp backwards by `rhs`.
    ///
    /// # Panics
    ///
    /// Panics if the result is outside the representable Unix timestamp range.
    fn sub(self, rhs: TimeSpan) -> Self::Output {
        self.checked_sub(rhs)
            .expect("system time subtraction overflow")
    }
}

/// Error returned when a system timestamp precedes the comparison timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemTimeError {
    duration: TimeSpan,
}

impl SystemTimeError {
    /// Returns how far the earlier timestamp is after the timestamp being tested.
    pub const fn duration(self) -> TimeSpan {
        self.duration
    }
}
