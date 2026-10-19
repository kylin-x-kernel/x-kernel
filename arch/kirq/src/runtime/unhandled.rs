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

#[cfg(all(test, not(unittest)))]
#[path = "../../tests/host/unhandled.rs"]
mod unhandled_unittest;

#[cfg(unittest)]
unittest::path_mod_test!(unhandled_unittest, "../../ut-tests/unhandled_unittest.rs");
