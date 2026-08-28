// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Workerqueue health checks.

use core::sync::atomic::{AtomicU64, Ordering};

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::builtinpool::{SystemPoolKind, system_pools};

const HEALTHCHECK_SLOTS: usize = SystemPoolKind::COUNT * kbuild_config::NR_CPUS;

static STUCK_SINCE_NS: [AtomicU64; HEALTHCHECK_SLOTS] =
    [const { AtomicU64::new(0) }; HEALTHCHECK_SLOTS];

/// Checks whether built-in system worker pools are making progress.
///
/// The watchdog path intentionally looks only at worker-pool snapshots. It uses
/// a non-blocking lock attempt because watchdog tasks may run from NMI context.
/// A contended pool lock is treated as a transient in-progress state for this
/// sample; it must not make the watchdog spin behind the code it is checking.
///
/// A pool is suspicious when runnable work exists but no worker is running,
/// preparing, claiming, or being created. The state must remain suspicious for
/// `threshold` before the check fails, so transient enqueue/wake windows do not
/// report a lockup.
pub fn system_workqueue_watchdog_check(now: MonotonicInstant, threshold: TimeSpan) -> bool {
    let now_ns = now.as_nanos_u64_saturating();
    let threshold_ns = threshold.as_nanos_u64_saturating();
    let mut healthy = true;

    for kind in SystemPoolKind::ALL {
        for cpu in 0..kbuild_config::NR_CPUS {
            let slot = kind.as_usize() * kbuild_config::NR_CPUS + cpu;
            let Some(pool) = system_pools().pool(kind, kcpu_id_map::LogicalCpuId::new(cpu)) else {
                STUCK_SINCE_NS[slot].store(0, Ordering::Release);
                continue;
            };
            let Some(snapshot) = pool.try_lock().map(|pool| pool.snapshot()) else {
                STUCK_SINCE_NS[slot].store(0, Ordering::Release);
                continue;
            };
            let has_progress_context = snapshot.nr_creating != 0
                || snapshot.nr_preparing != 0
                || snapshot.nr_claiming != 0
                || snapshot.nr_running_state != 0;
            let suspicious = snapshot.runnable != 0 && !has_progress_context;
            if !suspicious {
                STUCK_SINCE_NS[slot].store(0, Ordering::Release);
                continue;
            }

            let since = STUCK_SINCE_NS[slot].load(Ordering::Acquire);
            if since == 0 {
                STUCK_SINCE_NS[slot].store(now_ns, Ordering::Release);
                continue;
            }
            if now_ns.saturating_sub(since) >= threshold_ns {
                healthy = false;
            }
        }
    }

    healthy
}
