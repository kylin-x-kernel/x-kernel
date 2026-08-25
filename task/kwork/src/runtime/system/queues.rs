// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

use crate::WorkQueue;

/// Built-in system workerqueue kind.
///
/// This enum covers task-context system queues. Bottom-half queues are separate
/// runtime instances in `runtime::bh`; Linux-like creation flags such as
/// high-priority, unbound, freezable, and reclaim remain unsupported until
/// their backing policy exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemWorkQueueKind {
    /// Default global system queue for short common work.
    Default,
    /// Global queue for long-running work.
    ///
    /// This is a separate queue and flush/accounting domain from
    /// [`Self::Default`], but shares the same default per-CPU worker pool.
    Long,
}

impl SystemWorkQueueKind {
    /// All built-in system queue kinds.
    pub const ALL: [Self; Self::COUNT] = [Self::Default, Self::Long];
    /// Number of built-in system queue kinds.
    pub const COUNT: usize = 2;

    /// Returns the stable index used by provider-side per-kind arrays.
    pub const fn as_usize(self) -> usize {
        match self {
            Self::Default => 0,
            Self::Long => 1,
        }
    }

    /// Returns the built-in queue name.
    pub const fn queue_name(self) -> &'static str {
        match self {
            Self::Default => "system_wq",
            Self::Long => "system_long_wq",
        }
    }
}

static SYSTEM_WORKQUEUES: [WorkQueue; SystemWorkQueueKind::COUNT] = [
    WorkQueue::new("system_wq"),
    WorkQueue::new("system_long_wq"),
];

pub(super) struct SystemWorkQueues;

impl SystemWorkQueues {
    pub(super) fn for_kind(kind: SystemWorkQueueKind) -> &'static WorkQueue {
        &SYSTEM_WORKQUEUES[kind.as_usize()]
    }

    pub(super) fn cpu_is_valid(cpu_id: LogicalCpuId) -> bool {
        cpu_id.as_usize() < kbuild_config::NR_CPUS
    }

    pub(super) fn kind(queue: &'static WorkQueue) -> Option<SystemWorkQueueKind> {
        SystemWorkQueueKind::ALL
            .into_iter()
            .find(|&kind| core::ptr::eq(&SYSTEM_WORKQUEUES[kind.as_usize()], queue))
    }
}
