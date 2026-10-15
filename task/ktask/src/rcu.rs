// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task storage and scheduler integration for explicit read scopes.

use kcpu_id_map::LogicalCpuId;
use krcu::ReaderNode;

use crate::TaskInner;

mod service;
pub use service::init_service;

static READER_NODES: [ReaderNode; kbuild_config::NR_CPUS] =
    [const { ReaderNode::new() }; kbuild_config::NR_CPUS];

/// Called before the same-task decision and by the migration helper's path.
///
/// # Safety
/// `task` must be the current, Arc-owned task on `cpu`, with ordinary IRQs and
/// preemption excluded. Its storage must follow the task exit/GC protocol.
pub(crate) unsafe fn note_context_switch(task: &TaskInner, cpu: LogicalCpuId) {
    if let Some(callbacks) = service::callbacks() {
        // SAFETY: this is the outgoing current task under the caller's IRQ and
        // preemption exclusion. Task retirement retains its embedded reader;
        // the service reports only after enrollment and does not wake inline.
        unsafe { callbacks.note_context_switch(cpu.as_usize(), &task.rcu_reader) };
        return;
    }
    // SAFETY: switch_to runs with IRQs/preemption excluded for its current task.
    // Static nodes never move; the task remains Arc-owned while runnable, and
    // finish_task_switch unlinks exited readers before releasing the old owner.
    unsafe { READER_NODES[cpu.as_usize()].enroll(&task.rcu_reader) };
}

#[cfg(unittest)]
pub(crate) fn reader_node(cpu: LogicalCpuId) -> &'static ReaderNode {
    &READER_NODES[cpu.as_usize()]
}

unittest::path_mod_test!(tests_service_startup, "../ut-tests/rcu_service_startup.rs");
