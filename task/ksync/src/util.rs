// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Utility types and functions for synchronization primitives.

pub use kmutex::SpinConfig;
use ktask::yield_now;

/// Helper for adaptive spinning with configurable strategy.
pub(crate) struct Spin {
    count: u32,
    config: SpinConfig,
}

impl Spin {
    #[inline]
    pub(crate) fn new(config: SpinConfig) -> Self {
        Self { count: 0, config }
    }

    /// Perform one spin iteration.
    /// Returns `true` if more spins should be attempted, `false` if should block.
    ///
    /// Uses exponential backoff for the first `spin_before_yield` iterations.
    /// The maximum shift is `spin_before_yield`, so with typical values (<=10),
    /// this is safe from overflow.
    #[inline]
    pub(crate) fn spin(&mut self) -> bool {
        if self.count >= self.config.max_spins {
            return false;
        }
        self.count += 1;
        if self.count <= self.config.spin_before_yield {
            // Exponential backoff: 1 << count iterations
            // Safe for count <= 10 (typical), gives at most 1024 spins
            for _ in 0..(1 << self.count) {
                core::hint::spin_loop();
            }
        } else {
            yield_now();
        }
        true
    }
}
