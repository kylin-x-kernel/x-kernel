// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Steal one Ready `!on_cpu` waiter onto a CPU that is about to idle.

use core::sync::atomic::Ordering;

use kcpu_id_map::LogicalCpuId;
use ksched::EnqueueKind;

use super::{
    KCpuMask, RUN_QUEUES, RunQueue, get_run_queue,
    migrate::migrate_entry,
    placement::{can_idle_pull_task, find_idle_pull_src},
};
use crate::KtaskRef;

impl RunQueue {
    /// Steal one Ready `!on_cpu` waiter from a busier CPU onto this idle RQ.
    ///
    /// Locks only the source scheduler (then this RQ's enqueue lock). Never
    /// dest-then-src: that deadlocks against remote wake. Never steals
    /// `on_cpu` tasks (yield/preempt are already in the ready tree).
    pub(super) fn idle_pull(&self) -> bool {
        let dest = self.cpu_id.as_usize();
        let nr_cpus = kcpu_id_map::nr_cpus();
        let mut tried = KCpuMask::new();
        loop {
            let Some(src_idx) = find_idle_pull_src(dest, &tried, nr_cpus, |cpu| {
                RUN_QUEUES
                    .try_get(cpu)
                    .map(|rq| rq.nr_running.load(Ordering::Relaxed))
                    .unwrap_or(0)
            }) else {
                return false;
            };
            tried.set(src_idx, true);
            debug_assert_ne!(src_idx, dest);
            let Some(stolen) = get_run_queue(src_idx).steal_ready_for_idle_pull(self.cpu_id) else {
                continue;
            };
            if self.commit_idle_pull(stolen) {
                #[cfg(feature = "sched_stat")]
                super::stats::sched_stat_inc(&super::stats::sched_stat_cpu(self.cpu_id).idle_pull);
                return true;
            }
        }
    }

    /// Enqueue a stolen waiter, or send it elsewhere if affinity forbids dest.
    ///
    /// Steal to enqueue is not atomic: the task is Ready, on no RQ, with
    /// `cpu_id` still at src. `set_task_affinity` in that window is mask-only;
    /// `enqueue_task` rejects it under the dest lock.
    fn commit_idle_pull(&self, stolen: KtaskRef) -> bool {
        match self.enqueue_task(stolen) {
            Ok(_) => true,
            Err(rejected) => {
                migrate_entry(rejected, EnqueueKind::MigrateIn);
                false
            }
        }
    }

    /// Dequeue a stealable waiter from this (source) RQ. Caller must not hold
    /// the destination scheduler lock.
    fn steal_ready_for_idle_pull(&self, dest_cpu: LogicalCpuId) -> Option<KtaskRef> {
        let dest_idx = dest_cpu.as_usize();
        let now_ns = khal::time::monotonic_time_nanos();
        let stolen = {
            let _elapsed = Self::advance_account_epoch(&self.last_accounted_ns, now_ns);
            self.with_sched_rank(|s| {
                s.steal(&mut |task| can_idle_pull_task(task, dest_idx), now_ns)
            })?
        };
        Some(stolen)
    }
}
