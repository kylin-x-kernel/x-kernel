// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use extern_trait::extern_trait;
use kcpu_id_map::LogicalCpuId;
use ktask::{TaskExt, TaskInner};

use super::Thread;

/// Helper trait to access the thread from a task.
pub trait AsThread {
    /// Tries to get the thread from the task.
    fn try_as_thread(&self) -> Option<&Thread>;

    /// Returns the thread from the task, panicking if it is a kernel task.
    fn as_thread(&self) -> &Thread {
        self.try_as_thread().expect("kernel task")
    }
}

#[extern_trait]
// SAFETY: `Box<Thread>` is the canonical user-task extension payload in
// `kprocess`. The implementation only forwards scheduler hooks to the owning
// thread object and does not relax any `TaskExt` invariants.
unsafe impl TaskExt for Box<Thread> {
    fn set_user_mm_resident_cpu(&self, cpu_id: LogicalCpuId) {
        self.set_process_mm_resident_cpu(cpu_id);
    }

    fn switch_page_table_root(&self) -> Option<karch::HwPageTableRoot> {
        #[cfg(target_arch = "aarch64")]
        {
            Some(self.process_page_table_hw_root())
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            None
        }
    }
}

impl AsThread for TaskInner {
    fn try_as_thread(&self) -> Option<&Thread> {
        self.task_ext().map(|ext| {
            // SAFETY: user tasks in `kprocess` install `Box<Thread>` as their
            // `TaskExt` payload before publication; kernel tasks have no task
            // extension and are filtered out by the outer `Option`.
            unsafe { ext.downcast_ref::<Box<Thread>>() }.as_ref()
        })
    }
}
