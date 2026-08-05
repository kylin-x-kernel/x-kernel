// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    fmt,
    marker::PhantomData,
    ops::{Add, Sub},
};

use crate::TimeSpan;

/// Marker for the monotonic clock domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Monotonic {}

/// Marker for the boot-time clock domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Boottime {}

/// Marker for the process CPU-time clock domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProcessCpu {}

/// Marker for the thread CPU-time clock domain.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ThreadCpu {}

/// A point in the clock domain identified by `C`.
#[repr(transparent)]
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Instant<C> {
    since_origin: TimeSpan,
    clock: PhantomData<fn() -> C>,
}

impl<C> Instant<C> {
    /// The origin of this clock domain.
    pub const ORIGIN: Self = Self::from_span_since_origin(TimeSpan::ZERO);

    /// Creates an instant from elapsed time since this clock domain's origin.
    ///
    /// This constructor is intended for clock providers and explicit adapter
    /// boundaries. Ordinary subsystem code should obtain instants from its clock.
    pub const fn from_span_since_origin(since_origin: TimeSpan) -> Self {
        Self {
            since_origin,
            clock: PhantomData,
        }
    }

    /// Returns the elapsed time since this clock domain's origin.
    pub const fn span_since_origin(self) -> TimeSpan {
        self.since_origin
    }

    /// Returns nanoseconds since this clock's origin, saturating at `u64::MAX`.
    ///
    /// This method is intended only for hardware, ABI, serialization, and
    /// third-party adapter boundaries.
    pub const fn as_nanos_u64_saturating(self) -> u64 {
        self.since_origin.as_nanos_u64_saturating()
    }

    /// Advances this instant, returning `None` on overflow.
    pub const fn checked_add(self, duration: TimeSpan) -> Option<Self> {
        match self.since_origin.checked_add(duration) {
            Some(value) => Some(Self::from_span_since_origin(value)),
            None => None,
        }
    }

    /// Moves this instant backwards, returning `None` before the clock origin.
    pub const fn checked_sub(self, duration: TimeSpan) -> Option<Self> {
        match self.since_origin.checked_sub(duration) {
            Some(value) => Some(Self::from_span_since_origin(value)),
            None => None,
        }
    }

    /// Returns the elapsed time since `earlier`, or `None` if `earlier` is later.
    pub const fn checked_duration_since(self, earlier: Self) -> Option<TimeSpan> {
        self.since_origin.checked_sub(earlier.since_origin)
    }

    /// Returns the elapsed time since `earlier`, saturating at zero.
    pub const fn saturating_duration_since(self, earlier: Self) -> TimeSpan {
        self.since_origin.saturating_sub(earlier.since_origin)
    }
}

impl<C> fmt::Debug for Instant<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Instant").field(&self.since_origin).finish()
    }
}

impl<C> Add<TimeSpan> for Instant<C> {
    type Output = Self;

    /// Advances the instant by `rhs`.
    ///
    /// # Panics
    ///
    /// Panics if the resulting instant is not representable. Use
    /// [`Instant::checked_add`] for values derived from external input.
    fn add(self, rhs: TimeSpan) -> Self::Output {
        self.checked_add(rhs).expect("instant addition overflow")
    }
}

impl<C> Sub<TimeSpan> for Instant<C> {
    type Output = Self;

    /// Moves the instant backwards by `rhs`.
    ///
    /// # Panics
    ///
    /// Panics if the result precedes the clock origin. Use
    /// [`Instant::checked_sub`] when the ordering is not an invariant.
    fn sub(self, rhs: TimeSpan) -> Self::Output {
        self.checked_sub(rhs)
            .expect("instant subtraction precedes clock origin")
    }
}

impl<C> Sub for Instant<C> {
    type Output = TimeSpan;

    /// Returns the elapsed time between two instants in the same clock domain.
    ///
    /// # Panics
    ///
    /// Panics if `rhs` is later than `self`. Use
    /// [`Instant::checked_duration_since`] when the ordering is not an invariant.
    fn sub(self, rhs: Self) -> Self::Output {
        self.checked_duration_since(rhs)
            .expect("instant subtraction has reversed ordering")
    }
}

/// An instant in the monotonic clock domain.
pub type MonotonicInstant = Instant<Monotonic>;

/// An instant in the boot-time clock domain.
pub type BoottimeInstant = Instant<Boottime>;

/// An instant in the process CPU-time clock domain.
pub type ProcessCpuInstant = Instant<ProcessCpu>;

/// An instant in the thread CPU-time clock domain.
pub type ThreadCpuInstant = Instant<ThreadCpu>;
