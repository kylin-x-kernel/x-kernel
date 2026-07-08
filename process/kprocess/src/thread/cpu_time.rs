// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting state.

use khal::time::{TimeValue, monotonic_time_nanos};

/// Represents the current CPU-accounting state of a thread.
#[derive(Debug)]
pub enum CpuTimeState {
    /// The thread is not currently charging CPU time.
    None,
    /// The thread is executing in user space.
    User,
    /// The thread is executing in kernel space.
    Kernel,
}

/// Per-thread CPU-time accounting state.
pub struct CpuTimeStatistics {
    utime_ns: usize,
    stime_ns: usize,
    last_wall_ns: Option<usize>,
    state: CpuTimeState,
}

impl Default for CpuTimeStatistics {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuTimeStatistics {
    /// Creates a new [`CpuTimeStatistics`].
    pub fn new() -> Self {
        Self {
            utime_ns: 0,
            stime_ns: 0,
            last_wall_ns: None,
            state: CpuTimeState::None,
        }
    }

    /// Returns the current user time and system time as a tuple of [`TimeValue`].
    pub fn output(&self) -> (TimeValue, TimeValue) {
        let utime = TimeValue::from_nanos(self.utime_ns as u64);
        let stime = TimeValue::from_nanos(self.stime_ns as u64);
        (utime, stime)
    }

    /// Returns the sampled user and system CPU time in nanoseconds.
    pub fn sample_nanos(&mut self) -> (usize, usize) {
        self.update();
        (self.utime_ns, self.stime_ns)
    }

    /// Returns the sampled user and system CPU time as a tuple of [`TimeValue`].
    pub fn sample(&mut self) -> (TimeValue, TimeValue) {
        self.update();
        self.output()
    }

    fn update(&mut self) {
        let now_ns = monotonic_time_nanos() as usize;
        let Some(last_wall_ns) = self.last_wall_ns.replace(now_ns) else {
            return;
        };
        let delta = now_ns.saturating_sub(last_wall_ns);

        match self.state {
            CpuTimeState::User => {
                self.utime_ns += delta;
            }
            CpuTimeState::Kernel => {
                self.stime_ns += delta;
            }
            CpuTimeState::None => {}
        }
    }

    /// Updates the current CPU-accounting state.
    pub fn set_state(&mut self, state: CpuTimeState) {
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
