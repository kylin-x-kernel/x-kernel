// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use ktask::{TaskInner, UserTaskRuntime};

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

impl UserTaskRuntime for Thread {
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

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl AsThread for TaskInner {
    fn try_as_thread(&self) -> Option<&Thread> {
        self.user_runtime()?.as_any().downcast_ref::<Thread>()
    }
}
