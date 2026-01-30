//! Watchdog task registration and checks.
extern crate alloc;
use alloc::vec::Vec;

#[percpu::def_percpu]
static WATCHDOG_TASK_QUEUE: Vec<&'static dyn WatchdogTask> = Vec::new();

/// Watchdog task trait.
pub trait WatchdogTask {
    /// Task name
    fn name(&self) -> &str;

    /// Check whether the task is healthy.
    /// Return `true` if healthy, `false` to trigger recovery actions.
    fn check(&self) -> bool;

    // Called when `check()` returns false.
    // Default: do nothing.
    // fn on_failure(&self);
}

/// Register a watchdog task for the current CPU.
///
/// This function adds the task into the per-CPU watchdog task queue.
pub fn register_watchdog_task(task: &'static dyn WatchdogTask) {
    unsafe {
        WATCHDOG_TASK_QUEUE.current_ref_mut_raw().push(task);
    }
}

/// Check watchdog tasks and return the first failed task ID if any.
pub(crate) fn check_watchdog_tasks() -> Option<&'static str> {
    unsafe {
        let queue = WATCHDOG_TASK_QUEUE.current_ref_mut_raw();
        for task in queue.iter() {
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
        ktask::check_mutex_deadlock(khal::time::now_ticks() as usize)
    }
}
