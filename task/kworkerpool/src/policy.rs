// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-pool lifecycle and accounting policy.

use ktime_types::TimeSpan;

/// Per-instance worker-pool lifecycle and accounting policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicy {
    min_workers: usize,
    initial_workers: usize,
    max_workers: usize,
    idle_retire_after: Option<TimeSpan>,
    create_retry_delay: TimeSpan,
    cpu_intensive_threshold: TimeSpan,
    manager_managed: bool,
    dynamic_create: bool,
    idle_retire: bool,
}

/// Explicit inputs used to create one worker-pool policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicyConfig {
    pub min_workers: usize,
    pub initial_workers: usize,
    pub max_workers: usize,
    pub idle_retire_after: Option<TimeSpan>,
    pub create_retry_delay: TimeSpan,
    pub cpu_intensive_threshold: TimeSpan,
    pub manager_managed: bool,
    pub dynamic_create: bool,
    pub idle_retire: bool,
}

impl WorkerPoolPolicy {
    /// Creates a policy with explicit fields.
    ///
    /// Worker counts are normalized into `min <= initial <= max`. Growth and
    /// idle-retire switches are also disabled when the normalized count range
    /// cannot make those policies observable.
    pub const fn new(config: WorkerPoolPolicyConfig) -> Self {
        let max_workers = config.max_workers;
        let min_workers = if config.min_workers > max_workers {
            max_workers
        } else {
            config.min_workers
        };
        let initial_workers = if config.initial_workers < min_workers {
            min_workers
        } else if config.initial_workers > max_workers {
            max_workers
        } else {
            config.initial_workers
        };
        let dynamic_create = config.dynamic_create && min_workers < max_workers;
        let idle_retire =
            config.idle_retire && min_workers < max_workers && config.idle_retire_after.is_some();
        let idle_retire_after = if idle_retire {
            config.idle_retire_after
        } else {
            None
        };

        Self {
            min_workers,
            initial_workers,
            max_workers,
            idle_retire_after,
            create_retry_delay: config.create_retry_delay,
            cpu_intensive_threshold: config.cpu_intensive_threshold,
            manager_managed: config.manager_managed,
            dynamic_create,
            idle_retire,
        }
    }

    /// Minimum installed workers retained by this pool.
    pub const fn min_workers(self) -> usize {
        self.min_workers
    }

    /// Number of workers expected during initial pool bring-up.
    pub const fn initial_workers(self) -> usize {
        self.initial_workers
    }

    /// Maximum runtime worker slots.
    pub const fn max_workers(self) -> usize {
        self.max_workers
    }

    /// Idle duration after which an idle worker may retire.
    pub const fn idle_retire_after(self) -> Option<TimeSpan> {
        self.idle_retire_after
    }

    /// Retry delay after worker creation fails.
    pub const fn create_retry_delay(self) -> TimeSpan {
        self.create_retry_delay
    }

    #[cfg(unittest)]
    pub(crate) fn set_create_retry_delay_for_tests(&mut self, delay: TimeSpan) {
        self.create_retry_delay = delay;
    }

    /// Runtime after which a current execution stops counting toward pool
    /// concurrency.
    pub const fn cpu_intensive_threshold(self) -> TimeSpan {
        self.cpu_intensive_threshold
    }

    /// Returns whether a runtime manager handles slow-path lifecycle actions.
    pub const fn manager_managed(self) -> bool {
        self.manager_managed
    }

    #[cfg(unittest)]
    pub(crate) fn set_cpu_intensive_threshold_for_tests(&mut self, threshold: TimeSpan) {
        self.cpu_intensive_threshold = threshold;
    }

    /// Returns whether this pool may dynamically create runtime workers.
    pub const fn dynamic_create(self) -> bool {
        self.dynamic_create
    }

    /// Returns whether idle workers may retire.
    pub const fn idle_retire(self) -> bool {
        self.idle_retire
    }
}
