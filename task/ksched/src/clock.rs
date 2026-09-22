// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-RQ absolute monotonic clock. Class placement uses this timeline.

/// Last charged instant and the current `now_ns` snapshot.
#[derive(Clone, Copy, Debug)]
pub struct RqClock {
    now_ns: u64,
    last_ns: u64,
}

impl RqClock {
    /// Start the clock at `now_ns`.
    pub const fn new(now_ns: u64) -> Self {
        Self {
            now_ns,
            last_ns: now_ns,
        }
    }

    /// Advance to `now_ns` and return elapsed. A backwards sample yields 0.
    pub fn advance(&mut self, now_ns: u64) -> u64 {
        let elapsed = now_ns.saturating_sub(self.last_ns);
        if now_ns >= self.last_ns {
            self.last_ns = now_ns;
        }
        self.now_ns = now_ns.max(self.now_ns);
        elapsed
    }

    /// Last absolute sample.
    pub const fn now_ns(&self) -> u64 {
        self.now_ns
    }
}
