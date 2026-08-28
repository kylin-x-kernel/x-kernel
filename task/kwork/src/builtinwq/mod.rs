// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in system workqueue objects owned by `kwork`.

use crate::WorkQueue;

/// Built-in task-context system workqueues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemWorkQueueKind {
    Default,
}

/// Built-in bottom-half workqueues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BottomHalfWorkQueueKind {
    Default,
    HighPri,
}

static SYSTEM_WQ: WorkQueue = WorkQueue::system("system_wq", SystemWorkQueueKind::Default);
static SYSTEM_PERCPU_WQ: WorkQueue =
    WorkQueue::system_percpu("system_percpu_wq", SystemWorkQueueKind::Default);
static SYSTEM_BH_WQ: WorkQueue =
    WorkQueue::bottom_half("system_bh_wq", BottomHalfWorkQueueKind::Default);
static SYSTEM_BH_HIGHPRI_WQ: WorkQueue =
    WorkQueue::bottom_half("system_bh_highpri_wq", BottomHalfWorkQueueKind::HighPri);

pub fn system_wq() -> &'static WorkQueue {
    &SYSTEM_WQ
}

pub fn system_percpu_wq() -> &'static WorkQueue {
    &SYSTEM_PERCPU_WQ
}

pub fn system_bh_wq() -> &'static WorkQueue {
    &SYSTEM_BH_WQ
}

pub fn system_bh_highpri_wq() -> &'static WorkQueue {
    &SYSTEM_BH_HIGHPRI_WQ
}

pub(crate) fn system_queue(kind: SystemWorkQueueKind) -> &'static WorkQueue {
    match kind {
        SystemWorkQueueKind::Default => system_wq(),
    }
}

pub(crate) fn bottom_half_queue(kind: BottomHalfWorkQueueKind) -> &'static WorkQueue {
    match kind {
        BottomHalfWorkQueueKind::Default => system_bh_wq(),
        BottomHalfWorkQueueKind::HighPri => system_bh_highpri_wq(),
    }
}
