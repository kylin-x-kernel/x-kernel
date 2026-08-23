// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Workerqueue health checks over lock-free runtime statistics.

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{WorkerPoolStatsSnapshot, WorkqueueHostIf, runtime::system_pool_for_cpu};

/// Checks whether the current CPU's shared system worker pool is making progress.
///
/// This is intended for watchdog/NMI diagnostics. It consumes only lock-free
/// worker-pool statistics snapshots; it does not take the pool lock or inspect
/// the pending ring directly.
pub fn system_workqueue_watchdog_check(now: MonotonicInstant, threshold: TimeSpan) -> bool {
    let cpu_id = WorkqueueHostIf::current_cpu_id();
    system_pool_for_cpu(cpu_id)
        .is_none_or(|pool| worker_pool_backlog_is_healthy(pool.stats_snapshot(), now, threshold))
}

fn worker_pool_backlog_is_healthy(
    stats: WorkerPoolStatsSnapshot,
    now: MonotonicInstant,
    threshold: TimeSpan,
) -> bool {
    if stats.runnable_count == 0 {
        return true;
    }

    let Some(runnable_since) = stats.runnable_since else {
        return true;
    };

    let healthy_since = stats
        .last_progress
        .unwrap_or(runnable_since)
        .max(runnable_since);
    now.saturating_duration_since(healthy_since) <= threshold
}
