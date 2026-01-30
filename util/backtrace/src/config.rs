// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Configuration for backtrace operations.

use core::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

/// Configuration for backtrace capturing and unwinding.
#[derive(Debug, Clone)]
pub struct BacktraceConfig {
    /// Valid instruction pointer range.
    pub ip_range: Range<usize>,

    /// Valid frame pointer range.
    pub fp_range: Range<usize>,

    /// Maximum stack unwinding depth.
    pub max_depth: usize,

    /// Maximum stack size (in bytes) to prevent runaway unwinding.
    pub max_stack_size: usize,
}

impl BacktraceConfig {
    /// Create a new configuration with the given ranges.
    pub const fn new(ip_range: Range<usize>, fp_range: Range<usize>) -> Self {
        Self {
            ip_range,
            fp_range,
            max_depth: DEFAULT_MAX_DEPTH,
            max_stack_size: DEFAULT_MAX_STACK_SIZE,
        }
    }

    /// Validate a frame pointer against this configuration.
    pub fn validate_fp(&self, fp: usize) -> bool {
        self.fp_range.contains(&fp)
    }

    /// Validate an instruction pointer against this configuration.
    pub fn validate_ip(&self, ip: usize) -> bool {
        self.ip_range.contains(&ip)
    }
}

const DEFAULT_MAX_DEPTH: usize = 32;
const DEFAULT_MAX_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MB

/// Global maximum depth for stack unwinding (configurable at runtime).
static MAX_DEPTH: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_DEPTH);

/// Sets the maximum depth for stack unwinding.
pub fn set_max_depth(depth: usize) {
    if depth > 0 {
        MAX_DEPTH.store(depth, Ordering::Relaxed);
    }
}

/// Returns the current maximum depth for stack unwinding.
pub fn max_depth() -> usize {
    MAX_DEPTH.load(Ordering::Relaxed)
}
