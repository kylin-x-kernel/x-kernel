// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validated absolute deadlines and immutable delivery snapshots.

use kerrno::{KError, KResult};
use ktime_types::MonotonicInstant;

/// An absolute deadline representable by the core's nanosecond clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Deadline(u64);

impl Deadline {
    pub(super) const ZERO: Self = Self(0);

    pub(super) fn new(deadline: MonotonicInstant) -> KResult<Self> {
        deadline
            .span_since_origin()
            .try_as_nanos_u64()
            .map(Self)
            .ok_or(KError::InvalidInput)
    }

    pub(super) fn deadline_ns(self) -> u64 {
        self.0
    }
}

/// Immutable snapshot of the registration selected for this delivery.
/// A concurrent replacement cannot change this value. Repetition and business
/// state belong to the handler; the core does not accumulate expiration counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Expiration {
    /// Absolute monotonic deadline in nanoseconds for this delivery.
    pub deadline_ns: u64,
}

#[unittest::mod_test]
mod tests {
    use ktime_types::TimeSpan;
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn deadline_accepts_zero_and_horizon_but_rejects_out_of_range() {
        for ns in [0, 1, u64::MAX - 1, u64::MAX] {
            let instant = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns));
            assert_eq!(Deadline::new(instant).unwrap().deadline_ns(), ns);
        }
        let beyond = TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap();
        assert_eq!(
            Deadline::new(MonotonicInstant::from_span_since_origin(beyond)),
            Err(KError::InvalidInput)
        );
    }
}
