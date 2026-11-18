// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting state.

use ktime_types::{MonotonicInstant, TimeSpan};

/// Represents the current CPU-accounting mode of a thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuTimeState {
    /// The thread is off CPU or has finished CPU accounting.
    Inactive,
    /// The thread is executing in user space.
    User,
    /// The thread is executing in kernel space.
    Kernel,
}

/// Per-thread CPU time settled at execution-mode changes and on demand.
///
/// The owning thread serializes transitions and cross-CPU samples with its
/// accounting lock so each elapsed interval is charged to a single mode.
pub(crate) struct CpuTimeStatistics {
    utime: TimeSpan,
    stime: TimeSpan,
    last_update: Option<MonotonicInstant>,
    mode: CpuTimeState,
    finalized: bool,
}

impl CpuTimeStatistics {
    /// Creates inactive accounting; the first switch-in establishes its origin.
    pub(crate) const fn new() -> Self {
        Self {
            utime: TimeSpan::ZERO,
            stime: TimeSpan::ZERO,
            last_update: None,
            mode: CpuTimeState::Inactive,
            finalized: false,
        }
    }

    /// Settles the old mode before starting a new interval.
    pub(crate) fn switch_mode(&mut self, mode: CpuTimeState, now: MonotonicInstant) {
        self.settle(now);
        self.mode = mode;
    }

    fn settle(&mut self, now: MonotonicInstant) {
        let Some(last_update) = self.last_update.replace(now) else {
            return;
        };
        let elapsed = now.saturating_duration_since(last_update);
        match self.mode {
            CpuTimeState::User => self.utime = self.utime.saturating_add(elapsed),
            CpuTimeState::Kernel => self.stime = self.stime.saturating_add(elapsed),
            CpuTimeState::Inactive => {}
        }
    }

    /// Starts accounting for a thread that has just switched in.
    pub(crate) fn activate(&mut self, now: MonotonicInstant) {
        // PI cleanup may still schedule before exit publishes the final sample.
        if !self.finalized {
            self.switch_mode(CpuTimeState::Kernel, now);
        }
    }

    /// Closes accounting for exit. Later switch-ins do not reopen it.
    pub(crate) fn finalize(&mut self, now: MonotonicInstant) {
        self.deactivate(now);
        self.finalized = true;
    }

    /// Settles the running interval and stops charging while off CPU.
    pub(crate) fn deactivate(&mut self, now: MonotonicInstant) {
        self.switch_mode(CpuTimeState::Inactive, now);
    }

    /// Settles up to `now` and returns the accumulated user and kernel time.
    pub(crate) fn sample(&mut self, now: MonotonicInstant) -> (TimeSpan, TimeSpan) {
        self.settle(now);
        (self.utime, self.stime)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/thread_cpu_time.rs");
