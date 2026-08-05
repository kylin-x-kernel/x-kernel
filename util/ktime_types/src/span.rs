// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{fmt, time::Duration};

use crate::NANOS_PER_SEC;

/// A non-negative length of time.
///
/// `TimeSpan` intentionally requires explicit conversion to and from
/// [`core::time::Duration`], preventing APIs from silently mixing the kernel's
/// semantic time values with dependency-specific duration types.
#[repr(transparent)]
#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimeSpan(Duration);

impl TimeSpan {
    /// The largest representable time span.
    pub const MAX: Self = Self(Duration::MAX);
    /// A time span of zero length.
    pub const ZERO: Self = Self(Duration::ZERO);

    /// Creates a time span from whole seconds.
    pub const fn from_secs(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    /// Creates a time span from milliseconds.
    pub const fn from_millis(milliseconds: u64) -> Self {
        Self(Duration::from_millis(milliseconds))
    }

    /// Creates a time span from microseconds.
    pub const fn from_micros(microseconds: u64) -> Self {
        Self(Duration::from_micros(microseconds))
    }

    /// Creates a time span from nanoseconds.
    pub const fn from_nanos(nanoseconds: u64) -> Self {
        Self(Duration::from_nanos(nanoseconds))
    }

    /// Creates a time span from nanoseconds if the value is representable.
    pub const fn try_from_nanos(nanoseconds: u128) -> Option<Self> {
        let seconds = nanoseconds / NANOS_PER_SEC as u128;
        if seconds > u64::MAX as u128 {
            return None;
        }
        Some(Self::new(
            seconds as u64,
            (nanoseconds % NANOS_PER_SEC as u128) as u32,
        ))
    }

    /// Creates a time span from normalized second and nanosecond components.
    pub const fn new(seconds: u64, nanoseconds: u32) -> Self {
        Self(Duration::new(seconds, nanoseconds))
    }

    /// Explicitly wraps a Rust core duration.
    pub const fn from_core(duration: Duration) -> Self {
        Self(duration)
    }

    /// Explicitly returns the wrapped Rust core duration.
    pub const fn into_core(self) -> Duration {
        self.0
    }

    /// Returns `true` if this time span is zero.
    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Returns the whole seconds in this time span.
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    /// Returns the fractional nanosecond component.
    pub const fn subsec_nanos(self) -> u32 {
        self.0.subsec_nanos()
    }

    /// Returns the fractional microsecond component.
    pub const fn subsec_micros(self) -> u32 {
        self.0.subsec_micros()
    }

    /// Returns the total number of milliseconds.
    pub const fn as_millis(self) -> u128 {
        self.0.as_millis()
    }

    /// Returns the total number of microseconds.
    pub const fn as_micros(self) -> u128 {
        self.0.as_micros()
    }

    /// Returns the total number of nanoseconds.
    pub const fn as_nanos(self) -> u128 {
        self.0.as_nanos()
    }

    /// Returns the total nanoseconds if they fit in `u64`.
    pub const fn try_as_nanos_u64(self) -> Option<u64> {
        let nanos = self.as_nanos();
        if nanos <= u64::MAX as u128 {
            Some(nanos as u64)
        } else {
            None
        }
    }

    /// Returns total nanoseconds, saturating at `u64::MAX`.
    ///
    /// This is an explicit representation conversion for hardware, ABI,
    /// serialization, and third-party adapter boundaries.
    pub const fn as_nanos_u64_saturating(self) -> u64 {
        match self.as_secs().checked_mul(NANOS_PER_SEC) {
            Some(nanos) => nanos.saturating_add(self.subsec_nanos() as u64),
            None => u64::MAX,
        }
    }

    /// Adds two spans, returning `None` on overflow.
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Subtracts two spans, returning `None` if `rhs` is greater than `self`.
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Adds two spans, saturating at [`TimeSpan::MAX`].
    pub const fn saturating_add(self, rhs: Self) -> Self {
        match self.checked_add(rhs) {
            Some(value) => value,
            None => Self::MAX,
        }
    }

    /// Subtracts two spans, saturating at [`TimeSpan::ZERO`].
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        match self.checked_sub(rhs) {
            Some(value) => value,
            None => Self::ZERO,
        }
    }
}

impl fmt::Debug for TimeSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
