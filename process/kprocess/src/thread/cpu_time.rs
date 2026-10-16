// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting state.

use ktime_types::{MonotonicInstant, TimeSpan};

/// Represents the current CPU-accounting state of a thread.
#[derive(Debug, Clone, Copy)]
pub enum CpuTimeState {
    /// The thread is off CPU or has finished CPU accounting.
    Inactive,
    /// The thread is executing in user space.
    User,
    /// The thread is executing in kernel space.
    Kernel,
}

/// Per-thread CPU-time accounting state, following Linux generic vtime.
pub(crate) struct CpuTimeStatistics {
    utime: TimeSpan,
    stime: TimeSpan,
    last_wall: Option<MonotonicInstant>,
    state: CpuTimeState,
}

impl Default for CpuTimeStatistics {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuTimeStatistics {
    /// Creates inactive accounting; the first switch-in establishes its origin.
    pub(crate) fn new() -> Self {
        Self {
            utime: TimeSpan::ZERO,
            stime: TimeSpan::ZERO,
            last_wall: None,
            state: CpuTimeState::Inactive,
        }
    }

    /// Returns the committed user and system CPU time.
    pub(crate) fn output(&self) -> (TimeSpan, TimeSpan) {
        (self.utime, self.stime)
    }

    /// Settles the elapsed interval and returns the accumulated CPU time.
    pub(crate) fn sample(&mut self, now: MonotonicInstant) -> (TimeSpan, TimeSpan) {
        self.update(now);
        self.output()
    }

    fn update(&mut self, now: MonotonicInstant) {
        let Some(last_wall) = self.last_wall.replace(now) else {
            return;
        };
        let delta = now.saturating_duration_since(last_wall);
        match self.state {
            CpuTimeState::User => self.utime = self.utime.saturating_add(delta),
            CpuTimeState::Kernel => self.stime = self.stime.saturating_add(delta),
            CpuTimeState::Inactive => {}
        }
    }

    /// Settles the old interval before starting the new accounting state.
    pub(crate) fn set_state(&mut self, state: CpuTimeState, now: MonotonicInstant) {
        self.update(now);
        self.state = state;
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/thread_cpu_time.rs");
