// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog task registration and checks.
use alloc::vec::Vec;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, Ordering},
};

use kcpu_slot::cpu_slot_cell;

struct WatchdogTaskList {
    tasks: UnsafeCell<Vec<&'static dyn WatchdogTask>>,
    published: AtomicBool,
}

impl WatchdogTaskList {
    const fn new() -> Self {
        Self {
            tasks: UnsafeCell::new(Vec::new()),
            published: AtomicBool::new(false),
        }
    }
}

// SAFETY: `tasks` is mutated only by the owning CPU during initialization,
// before `published` is stored with Release ordering. After publication every
// context, including NMI, has shared read-only access. The list and its task
// references live for the lifetime of the kernel.
unsafe impl Sync for WatchdogTaskList {}

// SAFETY: `new()` contains an allocation-free `Vec::new()` and an unpublished
// atomic flag. Registration mutates only the per-CPU copy before publication.
unsafe impl kcpu_slot::StaticSlotValue for WatchdogTaskList {}

cpu_slot_cell! {
    static WATCHDOG_TASK_LIST: crate::watchdog_task::WatchdogTaskList =
        crate::watchdog_task::WatchdogTaskList::new();
}

/// A task periodically checked by the per-CPU watchdog for liveness.
pub trait WatchdogTask: Sync {
    /// Task name
    fn name(&self) -> &str;

    /// Check whether the task is healthy.
    /// Return `true` if healthy, `false` to trigger recovery actions.
    fn check(&self) -> bool;
}

/// Register a watchdog task for the current CPU.
///
/// This function adds the task into the per-CPU watchdog task queue.
///
/// # Panics
/// Panics if the current CPU's task list has already been published, which
/// happens once NMI delivery has been armed for that CPU.
pub fn register_watchdog_task(task: &'static dyn WatchdogTask) {
    // SAFETY: per-CPU initialization is single-threaded on the owning CPU and
    // calls this only before `publish_watchdog_tasks`. NMI delivery is armed
    // only after publication, so no reader can overlap this mutation.
    unsafe {
        let list = WATCHDOG_TASK_LIST.shared_current();
        assert!(
            !list.published.load(Ordering::Relaxed),
            "watchdog task registration after publication"
        );
        (*list.tasks.get()).push(task);
    }
}

/// Publishes the current CPU's completed task list to its NMI handler.
#[cfg(feature = "nmi")]
pub(crate) fn publish_watchdog_tasks() {
    // SAFETY: per-CPU initialization is pinned to the owning CPU. All task-list
    // writes precede this Release store, and no further writes are permitted.
    let list = WATCHDOG_TASK_LIST.shared_current();
    assert!(
        !list.published.swap(true, Ordering::Release),
        "watchdog task list published more than once"
    );
}

/// Check watchdog tasks and return the first failed task ID if any.
#[cfg(feature = "nmi")]
pub(crate) fn check_watchdog_tasks() -> Option<&'static str> {
    // SAFETY: NMI execution cannot migrate. The Acquire observes every list
    // write preceding publication, and publication permanently closes the
    // mutation phase, so forming a shared slice cannot race a writer.
    unsafe {
        let list = WATCHDOG_TASK_LIST.shared_current();
        if !list.published.load(Ordering::Acquire) {
            return None;
        }
        for task in (&*list.tasks.get()).iter() {
            if !task.check() {
                return Some(task.name());
            }
        }
        None
    }
}

/// Default mutex deadlock watchdog task.
pub static MUTEX_DEADLOCK_CHECK: MutexDeadlockCheck = MutexDeadlockCheck;

/// Watchdog task that checks for mutex deadlocks.
pub struct MutexDeadlockCheck;

impl WatchdogTask for MutexDeadlockCheck {
    fn name(&self) -> &str {
        "MutexDeadlock"
    }

    fn check(&self) -> bool {
        ktask::check_mutex_deadlock()
    }
}
