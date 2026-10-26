// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Adaptive spinning policy for the blocking mutex.

use kiface_task_api::TaskExecutionIf;

/// Spin configuration shared by blocking synchronization primitives.
///
/// Prefer `max_spins` in 1..=100 and `spin_before_yield` no greater than
/// `max_spins`, typically at most 10. Backoff uses `1 << count`; callers must
/// keep the reached spin count below the shift operand's bit width.
#[derive(Debug, Clone, Copy)]
pub struct SpinConfig {
    /// Maximum number of spin iterations before blocking.
    pub max_spins: u32,
    /// Number of exponentially backed-off spins before yielding.
    pub spin_before_yield: u32,
}

impl Default for SpinConfig {
    fn default() -> Self {
        Self {
            max_spins: 10,
            spin_before_yield: 3,
        }
    }
}

pub(crate) struct Spin {
    count: u32,
    config: SpinConfig,
}

impl Spin {
    #[inline]
    pub(crate) fn new(config: SpinConfig) -> Self {
        Self { count: 0, config }
    }

    #[inline]
    pub(crate) fn spin(&mut self) -> bool {
        if self.count >= self.config.max_spins {
            return false;
        }
        self.count += 1;
        if self.count <= self.config.spin_before_yield {
            for _ in 0..(1 << self.count) {
                core::hint::spin_loop();
            }
        } else {
            TaskExecutionIf::yield_now();
        }
        true
    }
}
