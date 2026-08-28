// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker-pool kinds and policy constants.

use ktime_types::TimeSpan;
use kworkerpool::{PoolKind, WorkerPoolPolicy, WorkerPoolPolicyConfig};

/// Maximum worker slots in one built-in system pool.
pub(crate) const BUILTIN_WORKERS_PER_POOL: usize = kbuild_config::WORKQUEUE_WORKERS_PER_POOL;

/// Pending executor entries retained by one built-in system pool.
pub(crate) const BUILTIN_POOL_ENTRY_CAP: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

/// Built-in worker-pool execution kind selected by system queues.
///
/// The kind names execution context and policy template. It is not a logical
/// workqueue name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemPoolKind {
    /// Sleepable task-context worker pool.
    Normal,
    /// Bottom-half execution pool. It has no ktask-created worker lifecycle.
    Bh,
}

impl SystemPoolKind {
    /// All built-in pool kinds supported by the first-stage model.
    pub(crate) const ALL: [Self; Self::COUNT] = [Self::Normal, Self::Bh];
    /// Number of built-in pool kinds.
    pub(crate) const COUNT: usize = 2;

    /// Stable kind index for static per-CPU pool tables.
    pub(crate) const fn as_usize(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Bh => 1,
        }
    }

    /// Restores a built-in kind from its stable table index.
    pub(crate) const fn from_usize(value: usize) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::Bh),
            _ => None,
        }
    }

    pub(crate) const fn pool_kind(self) -> PoolKind {
        PoolKind::new(self.as_usize())
    }

    /// Returns whether a per-CPU manager should process lifecycle actions.
    pub(crate) const fn is_manager_managed(self) -> bool {
        match self {
            Self::Normal => true,
            Self::Bh => false,
        }
    }

    /// Runtime-visible worker-pool kind name.
    pub(crate) const fn pool_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Bh => "bh",
        }
    }
}

pub(crate) const fn builtin_policy(kind: SystemPoolKind) -> WorkerPoolPolicy {
    match kind {
        SystemPoolKind::Normal => WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
            min_workers: 1,
            initial_workers: 1,
            max_workers: BUILTIN_WORKERS_PER_POOL,
            idle_retire_after: Some(TimeSpan::from_secs(30)),
            create_retry_delay: TimeSpan::from_millis(10),
            cpu_intensive_threshold: TimeSpan::from_millis(10),
            manager_managed: true,
            dynamic_create: true,
            idle_retire: true,
        }),
        SystemPoolKind::Bh => WorkerPoolPolicy::new(WorkerPoolPolicyConfig {
            min_workers: 1,
            initial_workers: 1,
            max_workers: 1,
            idle_retire_after: None,
            create_retry_delay: TimeSpan::from_millis(0),
            cpu_intensive_threshold: TimeSpan::from_millis(10),
            manager_managed: false,
            dynamic_create: false,
            idle_retire: false,
        }),
    }
}
