// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduler provider for the priority-inheritance graph.
//!
//! Graph callbacks read only immutable identity, the embedded node and a
//! single atomic donation snapshot. Boost submission enters the owning run
//! queue only after the graph lock has been released.

use krtmutex::{PiHost, PiTaskNode};

use crate::KtaskRef;

/// Binds `krtmutex` to kernel tasks and their owning run queues.
///
/// Used by `ksync::RtMutex`. Locking requires sleepable kernel-task or
/// process-thread context; early boot and interrupt context are unsupported.
pub struct TaskPiHost;

impl PiHost for TaskPiHost {
    type Task = KtaskRef;

    fn current() -> KtaskRef {
        crate::current().clone()
    }

    fn same_task(a: &KtaskRef, b: &KtaskRef) -> bool {
        KtaskRef::ptr_eq(a, b)
    }

    fn node(task: &KtaskRef) -> &PiTaskNode<Self> {
        &task.pi_node
    }

    fn donor_priority(task: &KtaskRef) -> u8 {
        task.pi_donor_priority_snapshot()
    }

    fn set_boost(task: &KtaskRef, priority: u8) -> bool {
        let prio = if priority == 0 {
            ksched::RtPrio::NONE
        } else {
            // The graph only forwards donation snapshots in 0..=99.
            // A bad level must not silently clear an existing boost.
            let Ok(prio) = ksched::RtPrio::new(i32::from(priority)) else {
                return false;
            };
            prio
        };
        crate::run_queue::set_task_pi_boost(task, prio)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/pi.rs");
