// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Run-queue ownership and `nr_running` / `nr_home` snapshots.

use core::sync::atomic::Ordering;

use kcpu_id_map::LogicalCpuId;

use super::{RUN_QUEUES, RunQueue};
use crate::{KtaskRef, TaskInner};

impl RunQueue {
    /// Sole writer of [`TaskInner::cpu_id`] for run-queue ownership.
    ///
    /// Semantics of the ownership field:
    /// - runnable / running: CPU whose run queue currently owns the task
    /// - blocked: last owner CPU, used as wake-affinity preference by
    ///   [`super::placement::select_wake_run_queue`]
    ///
    /// Call sites outside `publish_task` / `enqueue_task` / `switch_to_local`
    /// are limited to boot bring-up before a run queue exists.
    #[inline]
    pub(super) fn set_owner_cpu(task: &TaskInner, cpu_id: LogicalCpuId) {
        task.set_cpu_id(cpu_id);
    }

    /// Attach this run queue as the task's owner before publish / enqueue / local switch.
    ///
    /// If the task already counted toward another RQ's [`Self::nr_home`], that
    /// occupancy moves here (wake-fallback / migrate-in / idle-pull). Block does not call
    /// this, so a sleeping worker keeps its home.
    #[inline]
    pub(super) fn attach_owner(&self, task: &KtaskRef) {
        let old = task.cpu_id();
        if old != self.cpu_id
            && task.clear_home_charged()
            && let Some(old_rq) = RUN_QUEUES.try_get(old.as_usize())
        {
            old_rq.nr_home.fetch_sub(1, Ordering::Relaxed);
        }
        Self::set_owner_cpu(task, self.cpu_id);
    }

    /// Includes `task` in this RQ's sticky-home occupancy if not already charged.
    pub(super) fn charge_home(&self, task: &KtaskRef) {
        if task.mark_home_charged() {
            self.nr_home.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drops `task` from this RQ's sticky-home occupancy if it was charged.
    pub(super) fn discharge_home(&self, task: &KtaskRef) {
        if task.clear_home_charged() {
            self.nr_home.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
