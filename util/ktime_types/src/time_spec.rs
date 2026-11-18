// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt;

use crate::{NANOS_PER_SEC, SystemTime, TimeSpan};

/// A signed, normalized time offset.
///
/// The representation matches the kernel ABI `timespec`: whole seconds are
/// signed and the nanosecond component is always in `0..NANOS_PER_SEC`. This
/// lets one value represent offsets before and after an epoch, including
/// negative offsets such as `-1.5s` (`seconds = -2`, `nanoseconds = 500_000_000`).
#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimeSpec {
    seconds: i64,
    nanoseconds: u32,
}

impl TimeSpec {
    /// The zero offset.
    pub const ZERO: Self = Self {
        seconds: 0,
        nanoseconds: 0,
    };

    /// Creates an offset from normalized second and nanosecond components.
    ///
    /// Returns `None` when `nanoseconds` is outside `0..NANOS_PER_SEC`.
    pub const fn from_parts(seconds: i64, nanoseconds: u32) -> Option<Self> {
        if nanoseconds >= NANOS_PER_SEC as u32 {
            return None;
        }
        Some(Self {
            seconds,
            nanoseconds,
        })
    }

    /// Creates an offset from signed total nanoseconds.
    pub const fn from_nanos(nanoseconds: i128) -> Option<Self> {
        let seconds = nanoseconds.div_euclid(NANOS_PER_SEC as i128);
        if seconds < i64::MIN as i128 || seconds > i64::MAX as i128 {
            return None;
        }
        Some(Self {
            seconds: seconds as i64,
            nanoseconds: nanoseconds.rem_euclid(NANOS_PER_SEC as i128) as u32,
        })
    }

    /// Returns the normalized whole seconds, rounded toward negative infinity.
    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    /// Returns the normalized fractional nanosecond component.
    pub const fn subsec_nanos(self) -> u32 {
        self.nanoseconds
    }

    /// Returns the total signed nanoseconds.
    pub const fn as_nanos(self) -> i128 {
        self.seconds as i128 * NANOS_PER_SEC as i128 + self.nanoseconds as i128
    }

    /// Returns whether this offset is exactly zero.
    pub const fn is_zero(self) -> bool {
        self.seconds == 0 && self.nanoseconds == 0
    }

    /// Adds two offsets, returning `None` on overflow.
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        let total = self.as_nanos() + rhs.as_nanos();
        Self::from_nanos(total)
    }

    /// Subtracts `rhs` from this offset, returning `None` on overflow.
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        let total = self.as_nanos() - rhs.as_nanos();
        Self::from_nanos(total)
    }

    /// Applies this offset to a non-negative clock reading.
    ///
    /// The result is a signed timestamp-shaped value because a negative offset
    /// can move the reading before the clock's origin. Returns `None` when the
    /// adjusted value cannot be represented.
    pub const fn adjust_span(self, span: TimeSpan) -> Option<SystemTime> {
        let total = self.as_nanos() + span.as_nanos() as i128;
        SystemTime::from_unix_nanos(total)
    }
}

impl fmt::Debug for TimeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimeSpec")
            .field("seconds", &self.seconds)
            .field("nanoseconds", &self.nanoseconds)
            .finish()
    }
}
