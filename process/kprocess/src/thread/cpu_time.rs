// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting state.

use khal::time::monotonic_time;
use ktime_types::{MonotonicInstant, TimeSpan};

/// Represents the current CPU-accounting state of a thread.
#[derive(Debug, Clone, Copy)]
pub enum CpuTimeState {
    /// The thread is not currently charging CPU time.
    None,
    /// The thread is executing in user space.
    User,
    /// The thread is executing in kernel space.
    Kernel,
}

/// Per-thread CPU-time accounting state.
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
    /// Creates a new [`CpuTimeStatistics`].
    pub(crate) fn new() -> Self {
        Self {
            utime: TimeSpan::ZERO,
            stime: TimeSpan::ZERO,
            last_wall: None,
            state: CpuTimeState::None,
        }
    }

    /// Returns the current user time and system time as a tuple of [`TimeSpan`].
    pub(crate) fn output(&self) -> (TimeSpan, TimeSpan) {
        (self.utime, self.stime)
    }

    /// Returns the sampled user and system CPU time as a tuple of [`TimeSpan`].
    pub(crate) fn sample(&mut self) -> (TimeSpan, TimeSpan) {
        self.update();
        self.output()
    }

    fn update(&mut self) {
        let now = monotonic_time();
        let Some(last_wall) = self.last_wall.replace(now) else {
            return;
        };
        let delta = now.saturating_duration_since(last_wall);

        match self.state {
            CpuTimeState::User => {
                self.utime = self.utime.saturating_add(delta);
            }
            CpuTimeState::Kernel => {
                self.stime = self.stime.saturating_add(delta);
            }
            CpuTimeState::None => {}
        }
    }

    /// Updates the current CPU-accounting state.
    pub(crate) fn set_state(&mut self, state: CpuTimeState) {
        self.update();
        self.state = state;
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::{CpuTimeState, CpuTimeStatistics};

    #[def_test]
    fn test_timemanager_default_output() {
        let tm = CpuTimeStatistics::new();
        let (u, s) = tm.output();
        assert_eq!(u.as_secs(), 0);
        assert_eq!(u.subsec_nanos(), 0);
        assert_eq!(s.as_secs(), 0);
        assert_eq!(s.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_timemanager_set_state() {
        let mut tm = CpuTimeStatistics::new();
        tm.set_state(CpuTimeState::User);
        tm.set_state(CpuTimeState::Kernel);
        tm.set_state(CpuTimeState::None);
        let (u, s) = tm.output();
        assert_eq!(u.as_secs(), 0);
        assert_eq!(s.as_secs(), 0);
    }
}
