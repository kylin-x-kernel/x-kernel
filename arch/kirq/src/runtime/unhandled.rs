// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consecutive-unhandled accounting for descriptor-backed action fanout.

/// Consecutive unclaimed interrupts tolerated on one line before it is masked.
///
/// A line that has produced this many consecutive interrupts nobody claims is
/// not about to be claimed: either its handler was never installed or it
/// answers `NotHandled` for a source it does not clear. Leaving it enabled
/// costs the whole machine, not one peripheral. Matches the order of Linux's
/// `note_interrupt` threshold, which likewise resets its spurious count when a
/// dispatch is handled.
pub(super) const UNCLAIMED_LIMIT: u64 = 100_000;

/// Consecutive-event sampling state, serialized by the descriptor's owner.
///
/// A handled dispatch resets this state, so isolated unclaimed events do not
/// accumulate across correct service of the same line. Action changes and
/// enable/disable do not reset it. Recreating the descriptor also starts a new
/// count; no clock or hardware policy is used.
pub(super) struct UnhandledIrqState {
    count: u64,
    suppressed: u64,
}

/// An owned summary that can be logged after releasing the IRQ state lock.
pub(super) struct UnhandledIrqReport {
    pub(super) count: u64,
    pub(super) suppressed: u64,
    /// The count reached another [`UNCLAIMED_LIMIT`] boundary; the line must
    /// be masked rather than left to starve the machine.
    pub(super) mask: bool,
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
    /// Reports occur at counts 1, 2, 4, ..., at every [`UNCLAIMED_LIMIT`]
    /// boundary, and once when the count saturates, so a streak of `N` events
    /// emits at most `O(log N + N / UNCLAIMED_LIMIT)` reports instead of one
    /// per interrupt. Counts at `u64::MAX` are lower bounds; saturation never
    /// restarts the initial report burst.
    pub(super) fn record(&mut self) -> Option<UnhandledIrqReport> {
        let previous = self.count;
        self.count = self.count.saturating_add(1);
        let mask = self.count != previous && self.count.is_multiple_of(UNCLAIMED_LIMIT);
        if self.count != previous
            && (self.count.is_power_of_two() || self.count == u64::MAX || mask)
        {
            let report = UnhandledIrqReport {
                count: self.count,
                suppressed: self.suppressed,
                mask,
            };
            self.suppressed = 0;
            Some(report)
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }

    /// Clears the streak after a handled dispatch.
    ///
    /// The next unclaimed event starts a new report sequence at count 1, and
    /// only a new consecutive streak can reach [`UNCLAIMED_LIMIT`].
    pub(super) fn reset(&mut self) {
        self.count = 0;
        self.suppressed = 0;
    }

    #[cfg(unittest)]
    pub(super) const fn counts_for_tests(&self) -> (u64, u64) {
        (self.count, self.suppressed)
    }

    #[cfg(unittest)]
    pub(super) fn seed_for_tests(&mut self, count: u64, suppressed: u64) {
        self.count = count;
        self.suppressed = suppressed;
    }
}

#[cfg(all(test, not(unittest)))]
#[path = "../../tests/host/unhandled.rs"]
mod unhandled_unittest;

#[cfg(unittest)]
unittest::path_mod_test!(unhandled_unittest, "../../ut-tests/unhandled_unittest.rs");
