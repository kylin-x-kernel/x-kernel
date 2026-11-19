// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw CPU-time sample shared by accounting and CPU-timer expiry.

/// One raw CPU-time sample of a thread or thread group: the tick-classified
/// user/system split plus the scheduler runtime it is accounted against.
///
/// CPU-timer expiry consumes this raw sample directly, so all timers polled
/// from one snapshot share a single time base. Only stat/getrusage/wait-style
/// views scale the split against the runtime through a monotonic adjustment
/// history; the adjusted values never drive timer deadlines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskCputime {
    /// Raw user-mode CPU time in nanoseconds.
    pub utime: u64,
    /// Raw kernel-mode CPU time in nanoseconds.
    pub stime: u64,
    /// Scheduler execution runtime in nanoseconds.
    pub sum_exec_runtime: u64,
}

impl TaskCputime {
    /// Adds one member's raw split and scheduler runtime, saturating like the
    /// surrounding group ledger.
    pub fn add(&mut self, utime: u64, stime: u64, runtime: u64) {
        self.utime = self.utime.saturating_add(utime);
        self.stime = self.stime.saturating_add(stime);
        self.sum_exec_runtime = self.sum_exec_runtime.saturating_add(runtime);
    }
}
