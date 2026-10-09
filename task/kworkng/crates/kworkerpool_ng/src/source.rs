// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Retained demand for one queue/CPU source.
//!
//! Queued means the service should inspect a SOURCE, not that a Work is running.
//! Idle does not prove that the queue is empty or callbacks have completed.
//! A notice during inspection survives even when that inspection reports no work.
//! Inspection must end before any acquired callback runs.
//!
//! The dispatcher protects these transitions and source-list membership
//! with one IRQ-safe lock. Each returned insertion/removal decision must be applied
//! under that same lock. Inspection of management happens after releasing it.
//! These methods do not allocate, lock, wake, retain Queue or call business code.
//! State alone is not an implemented dispatcher or a race-free wakeup protocol.
//!
//! Detach prevents future selection, not accepted-work execution or cancellation.
//! The first detach owns accounting closure; repeated detach must not acknowledge
//! completion ahead of unlocked reference release. InspectingDetached preserves an
//! outstanding inspection turn until finish. Detached alone is not a quiescence
//! receipt: independent source retirement must cover outstanding access/release.

use crate::SourceRemainder;

/// Service inspection state, separate from Work pending/running and cleanup.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum PartitionDispatchState {
    Idle,
    Queued,
    Inspecting,
    InspectingNotified,
    InspectingDetached,
    Detached,
}

impl PartitionDispatchState {
    /// Starts an association without requesting inspection.
    pub(super) const fn new() -> Self {
        Self::Idle
    }

    /// Retains demand; true requires one source-list insertion under the same lock.
    pub(super) fn notify(&mut self) -> bool {
        match self {
            Self::Idle => {
                *self = Self::Queued;
                true
            }
            Self::Inspecting => {
                *self = Self::InspectingNotified;
                false
            }
            Self::Queued | Self::InspectingNotified | Self::InspectingDetached | Self::Detached => {
                false
            }
        }
    }

    /// Reserves one listed source's inspection turn, coupled with list removal.
    ///
    /// # Panics
    /// Panics unless this source is Queued.
    pub(super) fn begin_inspection(&mut self) {
        assert_eq!(*self, Self::Queued, "source is not available for selection");
        *self = Self::Inspecting;
    }

    /// Ends a turn; true requires one tail insertion before unlocking.
    ///
    /// A concurrent notice survives an Empty remainder; the next turn consumes that
    /// notice so it cannot create perpetual demand. Detached turns never requeue.
    ///
    /// # Panics
    /// Panics unless this source has an outstanding inspection turn.
    pub(super) fn finish_inspection(&mut self, remainder: SourceRemainder) -> bool {
        if *self == Self::InspectingDetached {
            *self = Self::Detached;
            return false;
        }
        let needs_requeue = match self {
            Self::Inspecting => remainder == SourceRemainder::More,
            Self::InspectingNotified => true,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: Callers finish only a turn \
                          started by begin_inspection, and notices or detach keep that turn in an \
                          inspecting state"
            )]
            _ => panic!("source has no selection to finish"),
        };
        *self = if needs_requeue {
            Self::Queued
        } else {
            Self::Idle
        };
        needs_requeue
    }

    /// Permanently stops selection; returns (remove_list_node, close_accounting).
    ///
    /// Only the first detach owns closure, performed after releasing removed
    /// references outside the service lock. An inspection still delays retirement.
    pub(super) fn detach(&mut self) -> (bool, bool) {
        match self {
            Self::Idle => {
                *self = Self::Detached;
                (false, true)
            }
            Self::Queued => {
                *self = Self::Detached;
                (true, true)
            }
            Self::Inspecting | Self::InspectingNotified => {
                *self = Self::InspectingDetached;
                (false, true)
            }
            Self::InspectingDetached | Self::Detached => (false, false),
        }
    }
}

#[cfg(unittest)]
mod tests;
