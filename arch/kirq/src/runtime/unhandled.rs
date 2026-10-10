// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Descriptor-lifetime accounting for unclaimed action fanout.

/// Event-count sampling state, serialized by the descriptor's owner.
///
/// Handled dispatches and action changes do not reset this state. Recreating
/// the descriptor starts a new lifetime; no clock or hardware policy is used.
pub(super) struct UnhandledIrqState {
    count: u64,
    suppressed: u64,
}

/// An owned summary that can be logged after releasing the IRQ state lock.
pub(super) struct UnhandledIrqReport {
    pub(super) count: u64,
    pub(super) suppressed: u64,
}

impl UnhandledIrqState {
    pub(super) const fn new() -> Self {
        Self {
            count: 0,
            suppressed: 0,
        }
    }

    /// Records one fully unclaimed dispatch and optionally schedules a report.
    ///
    /// Report at counts 1, 2, 4, ... and once when the count saturates. At most
    /// 65 reports are scheduled per descriptor lifetime. Counts at `u64::MAX`
    /// are lower bounds; saturation never restarts the initial report burst.
    pub(super) fn record(&mut self) -> Option<UnhandledIrqReport> {
        let previous = self.count;
        self.count = self.count.saturating_add(1);
        if self.count != previous && (self.count.is_power_of_two() || self.count == u64::MAX) {
            let report = UnhandledIrqReport {
                count: self.count,
                suppressed: self.suppressed,
            };
            self.suppressed = 0;
            Some(report)
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

#[cfg(any(test, unittest))]
#[cfg_attr(unittest, unittest::mod_test)]
mod tests {
    use super::UnhandledIrqState;

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, unittest::def_test)]
    fn first_and_power_of_two_reports_account_for_every_event() {
        let mut state = UnhandledIrqState::new();
        let mut observed = 0;
        for count in 1u64..=1024 {
            match state.record() {
                Some(report) => {
                    assert!(count.is_power_of_two());
                    assert_eq!(report.count, count);
                    observed += report.suppressed + 1;
                    assert_eq!(observed, count);
                }
                None => assert!(!count.is_power_of_two()),
            }
        }
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, unittest::def_test)]
    fn suppressed_tail_is_retained_until_the_next_report() {
        let mut state = UnhandledIrqState::new();
        for _ in 0..7 {
            let _ = state.record();
        }
        assert_eq!(state.count, 7);
        assert_eq!(state.suppressed, 3);
        let report = state.record().expect("eighth unhandled dispatch reports");
        assert_eq!(report.count, 8);
        assert_eq!(report.suppressed, 3);
        assert_eq!(state.suppressed, 0);
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, unittest::def_test)]
    fn every_power_of_two_is_a_single_report_boundary() {
        for bit in 1..64 {
            let boundary = 1u64 << bit;
            let mut state = UnhandledIrqState {
                count: boundary - 1,
                suppressed: boundary / 2 - 1,
            };
            let report = state.record().expect("power-of-two boundary reports");
            assert_eq!(report.count, boundary);
            assert_eq!(report.suppressed, boundary / 2 - 1);
            assert!(state.record().is_none());
        }
    }

    #[cfg_attr(test, test)]
    #[cfg_attr(unittest, unittest::def_test)]
    fn saturation_reports_once_and_never_wraps() {
        let mut state = UnhandledIrqState {
            count: u64::MAX - 1,
            suppressed: u64::MAX - (1u64 << 63) - 1,
        };
        let report = state.record().expect("first saturation reports");
        assert_eq!(report.count, u64::MAX);
        assert_eq!(report.suppressed, u64::MAX - (1u64 << 63) - 1);
        for _ in 0..16 {
            assert!(state.record().is_none());
        }
        assert_eq!(state.count, u64::MAX);
        state.suppressed = u64::MAX;
        assert!(state.record().is_none());
        assert_eq!(state.suppressed, u64::MAX);
    }
}
