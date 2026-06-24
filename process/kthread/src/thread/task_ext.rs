// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use extern_trait::extern_trait;
use ktask::{TaskExt, TaskInner};

use super::Thread;

/// Helper trait to access the thread from a task.
pub trait AsThread {
    /// Tries to get the thread from the task.
    fn try_as_thread(&self) -> Option<&Thread>;

    /// Returns the thread from the task, panicking if it is a kernel task.
    ///
    /// # Panics
    ///
    /// Panics if this task has no user-thread extension.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let thread = ktask::current().as_thread();
    /// let pid = thread.pid();
    /// ```
    fn as_thread(&self) -> &Thread {
        self.try_as_thread().expect("kernel task")
    }
}

// SAFETY: `Box<Thread>` is `Send` because thread state is not shared across
// threads during migration.
#[extern_trait]
unsafe impl TaskExt for Box<Thread> {
    fn switch_page_table_root(&self) -> Option<karch::HwPageTableRoot> {
        #[cfg(target_arch = "aarch64")]
        {
            Some(self.proc_state.runtime().page_table_hw_root())
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
            // SAFETY: The extension slot was populated with `Box<Thread>` during
            // thread creation (see `Thread::new`), so the concrete type is guaranteed
            // to match the `downcast_ref` call.
            unsafe { ext.downcast_ref::<Box<Thread>>() }.as_ref()
        })
    }
}
