// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task storage and scheduler integration for explicit read scopes.

use kcpu_id_map::LogicalCpuId;
use krcu::ReaderNode;

use crate::TaskInner;

static READER_NODES: [ReaderNode; kbuild_config::NR_CPUS] =
    [const { ReaderNode::new() }; kbuild_config::NR_CPUS];

/// Called before the same-task decision and by the migration helper's path.
///
/// # Safety
/// `task` must be the current, Arc-owned task on `cpu`, with ordinary IRQs and
/// preemption excluded. Its storage must follow the task exit/GC protocol.
pub(crate) unsafe fn note_context_switch(task: &TaskInner, cpu: LogicalCpuId) {
    // SAFETY: switch_to runs with IRQs/preemption excluded for its current task.
    // Static nodes never move; the task remains Arc-owned while runnable and GC
    // permanently retains exited tasks with residual read participation.
    unsafe { READER_NODES[cpu.as_usize()].enroll(&task.rcu_reader) };
}

#[cfg(unittest)]
pub(crate) fn reader_node(cpu: LogicalCpuId) -> &'static ReaderNode {
    &READER_NODES[cpu.as_usize()]
}
