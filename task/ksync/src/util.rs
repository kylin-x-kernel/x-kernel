// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Utility types and functions for synchronization primitives.

use ktask::yield_now;

/// Spin configuration for blocking synchronization primitives.
///
/// # Valid Ranges
///
/// - `max_spins`: Should be in the range 1..=100 for reasonable behavior
/// - `spin_before_yield`: Should be <= `max_spins` and typically <= 10 to avoid
///   excessive spin loop iterations (exponential backoff)
#[derive(Debug, Clone, Copy)]
pub struct SpinConfig {
    /// Maximum number of spin iterations before blocking
    pub max_spins: u32,
    /// Number of spins before yielding
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
