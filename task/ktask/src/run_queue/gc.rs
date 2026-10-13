// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Delayed reclaim of exited tasks.

use alloc::sync::Arc;
use core::{
    future::poll_fn,
    task::{Context, Poll},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
#[cfg(feature = "snapshot")]
use khal::percpu::this_cpu_id;
use kpoll::{PollRegistrations, PollSet};

use super::{current_wait_for_exit, with_exited_tasks, with_wait_for_exit};
use crate::{KCpuMask, KtaskRef, TaskInner, future::block_on};

/// Pins a per-CPU GC task and initializes [`WAIT_FOR_EXIT`] for this CPU.
pub(super) fn spawn_gc_task(cpu_id: LogicalCpuId) -> KtaskRef {
    let gc_task = TaskInner::new_internal(
        || {
            let mut registrations = PollRegistrations::new();
            block_on(poll_fn(move |cx| poll_gc(cx, &mut registrations)))
        },
        "gc".into(),
        kbuild_config::TASK_STACK_SIZE,
    )
    .into_arc();
    gc_task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    with_wait_for_exit(|wait_for_exit| {
        wait_for_exit.init_once(PollSet::new());
    });
    gc_task
}

pub(super) fn poll_gc(cx: &mut Context<'_>, registrations: &mut PollRegistrations) -> Poll<()> {
    loop {
        // Drop all exited tasks and recycle resources.
        let n = with_exited_tasks(|exited_tasks| exited_tasks.len());
        for _ in 0..n {
            // Do not do the slow drops in the critical section.
            let Some(task) = with_exited_tasks(|exited_tasks| exited_tasks.pop_front()) else {
                continue;
            };
            if task.rcu_reader.is_reading() || task.rcu_reader.is_enrolled() {
                // GC runs after this CPU has left the exited task. An abandoned
                // scope must not leave a dangling intrusive node or silently
                // lose protection. Retain the whole task and its owned resources
                // (including identity/runtime), without retrying forever.
                warn!(
                    "task {} exited with residual RCU protection; retaining storage",
                    task.id_name()
                );
                core::mem::forget(task);
                continue;
            }
            match Arc::try_unwrap(task) {
                Ok(task) => {
                    // If I'm the last holder of the task, drop it immediately.
                    drop(task);
                }
                Err(task) => {
                    // Otherwise (e.g, `switch_to` is not completed, held by the
                    // joiner, etc), push it back and wait for them to drop first.
                    with_exited_tasks(|exited_tasks| exited_tasks.push_back(task));
                }
            }
        }

        #[cfg(feature = "snapshot")]
        {
            let _g = kspin::NoPreempt::new();
            crate::task_registry::sweep_tracked_tasks(this_cpu_id());
        }

        // Note: we cannot block current task with preemption disabled,
        // use `current_ref_raw` to get the `WAIT_FOR_EXIT`'s reference here to avoid
        // the use of `NoPreemptGuard`. Since gc task is pinned to the current
        // CPU, there is no affection if the gc task is preempted during the process.
        let mut context = registrations.context(cx);
        if context.register(current_wait_for_exit()).is_err() {
            drop(context);
            // Retry after yielding rather than sleeping without a registration
            // or busy-spinning on `wake_by_ref`.
            crate::yield_now();
            continue;
        }
        drop(context);

        // New tasks might be added during the above section, recheck it to
        // prevent us from sleeping indefinitely.
        if with_exited_tasks(|exited_tasks| exited_tasks.is_empty()) {
            break;
        }

        crate::yield_now();
    }

    Poll::Pending
}
