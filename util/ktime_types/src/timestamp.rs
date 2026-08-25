// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{NANOS_PER_SEC, SystemTime};

/// Timestamp range and granularity supported by a storage or protocol format.
///
/// The value carries the same invariants as Linux
/// `super_block::{s_time_gran,s_time_min,s_time_max}`. Timestamps are clamped
/// to the inclusive range and rounded down to the declared nanosecond
/// granularity. The minimum and maximum are endpoint instants, so timestamps
/// at either boundary have a zero nanosecond component, matching Linux
/// `timestamp_truncate()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimestampLimits {
    granularity_ns: u32,
    minimum_seconds: i64,
    maximum_seconds: i64,
}

impl TimestampLimits {
    /// Nanosecond precision across the full [`SystemTime`] range.
    pub const NANOSECOND: Self = Self {
        granularity_ns: 1,
        minimum_seconds: i64::MIN,
        maximum_seconds: i64::MAX,
    };
    /// Linux's conservative superblock default of whole-second precision.
    pub const SECOND: Self = Self {
        granularity_ns: NANOS_PER_SEC as u32,
        minimum_seconds: i64::MIN,
        maximum_seconds: i64::MAX,
    };

    /// Creates timestamp limits with an inclusive seconds range.
    ///
    /// # Panics
    ///
    /// Panics if `granularity_ns` is zero or greater than one second, or if
    /// `minimum_seconds` is greater than `maximum_seconds`.
    pub const fn new(granularity_ns: u32, minimum_seconds: i64, maximum_seconds: i64) -> Self {
        assert!(granularity_ns > 0 && granularity_ns <= NANOS_PER_SEC as u32);
        assert!(minimum_seconds <= maximum_seconds);
        Self {
            granularity_ns,
            minimum_seconds,
            maximum_seconds,
        }
    }

    /// Returns the supported timestamp granularity in nanoseconds.
    pub const fn granularity_ns(self) -> u32 {
        self.granularity_ns
    }

    /// Returns the earliest supported Unix timestamp in seconds.
    pub const fn minimum_seconds(self) -> i64 {
        self.minimum_seconds
    }

    /// Returns the latest supported Unix timestamp in seconds.
    pub const fn maximum_seconds(self) -> i64 {
        self.maximum_seconds
    }

    /// Clamps and rounds a timestamp down to this capability.
    pub fn truncate(self, timestamp: SystemTime) -> SystemTime {
        let seconds = timestamp
            .unix_seconds()
            .clamp(self.minimum_seconds, self.maximum_seconds);
        let nanoseconds = if seconds == self.minimum_seconds || seconds == self.maximum_seconds {
            0
        } else {
            let nanoseconds = timestamp.subsec_nanos();
            nanoseconds - nanoseconds % self.granularity_ns
        };
        SystemTime::from_unix_parts(seconds, nanoseconds)
            .expect("validated timestamp granularity preserves normalized nanoseconds")
    }
}

impl Default for TimestampLimits {
    fn default() -> Self {
        Self::SECOND
    }
}
