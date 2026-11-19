// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cooperative freezing of an explicit, stable task set across present CPUs.
//!
//! A blocked task is not automatically safe. Only a checkpoint or task exit
//! acknowledges a request. This module never interrupts a syscall or parks an
//! arbitrary lock holder. [`freeze_tasks`] retains its explicit-set contract;
//! [`freeze_system_tasks`] closes scheduler admission and collects the complete
//! admitted set, with explicit kernel-infrastructure exemptions.

use alloc::{sync::Arc, vec::Vec};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use kerrno::KError;
use ktime_types::TimeSpan;

use crate::{KtaskRef, TaskState, completion::CompletionWait};

pub(crate) mod system;
pub use system::{SystemFrozenTasks, freeze_system_tasks, infrastructure_tasks};

static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Admission;
impl Admission {
    fn acquire() -> Result<Self, FreezeError> {
        ACTIVE
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| FreezeError::Busy)?;
        Ok(Self)
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

/// Admission, cancellation or missing cooperative acknowledgment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreezeError {
    /// Requires a sleepable task with enabled IRQs.
    InvalidContext,
    /// Another freeze attempt still owns its requests.
    Busy,
    /// Invalid explicit targets, or user/idle/self/duplicate/unregistered exemptions.
    InvalidTaskSet,
    /// The caller cancelled this attempt before all acknowledgments arrived.
    Cancelled,
    /// At least one task failed to reach a safe point before the deadline.
    TimedOut,
    /// Preparing the owned deadline timer failed before any request was published.
    Timer(KError),
}

struct Progress {
    // Release/acquire publishes cancellation before a waiter checks its predicate.
    is_cancelled: AtomicBool,
    changed: kcompletion::WaitQueue,
}

/// A one-shot cancellation handle, prepared in task context before freezing.
/// Clones may cancel from task or IRQ context. Use a fresh handle for a retry.
#[derive(Clone)]
pub struct FreezeCancellation(Arc<Progress>);

impl Default for FreezeCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl FreezeCancellation {
    /// Allocates the notification state; requires ordinary task context.
    pub fn new() -> Self {
        Self(Arc::new(Progress {
            is_cancelled: AtomicBool::new(false),
            changed: kcompletion::WaitQueue::new(),
        }))
    }

    /// Cancels an unfinished wait; an already returned frozen guard stays frozen
    /// until its owner drops it, so cancellation cannot release tasks under PM.
    pub fn cancel(&self) {
        self.0.is_cancelled.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }
}

struct Round {
    progress: Arc<Progress>,
    // Release/acquire plus the post-block recheck closes release-before-park.
    is_released: AtomicBool,
}

pub(crate) struct Request {
    round: Arc<Round>,
    // Published on the incoming stack, after the target has entered Frozen.
    is_confirmed: AtomicBool,
    // Distinguishes this request from a late checkpoint in a released round.
    has_entered: AtomicBool,
}

/// Owns confirmed freeze requests. Dropping thaws the set on success or abort.
/// Bound to the coordinating task's stack: it must not outlive PM recovery.
#[must_use = "dropping the guard releases every frozen task"]
pub struct FrozenTasks {
    requests: Vec<(KtaskRef, Arc<Request>)>,
    round: Arc<Round>,
    _task_bound: PhantomData<*mut ()>,
    _admission: Admission,
}

impl Drop for FrozenTasks {
    fn drop(&mut self) {
        // Remove every request before allowing another attempt. Move references
        // out of the slot lock so final destruction never occurs under it.
        for (task, _) in &self.requests {
            let request = task.freeze_request.lock().take();
            drop(request);
        }
        self.round.is_released.store(true, Ordering::Release);
        for (task, _) in &self.requests {
            crate::select_wake_run_queue::<kspin::NoPreemptIrqSave>(task).unblock_task_in_state(
                task.clone(),
                TaskState::Frozen,
                true,
            );
        }
    }
}

/// Requests and confirms a stable set of tasks before its owner may suspend.
///
/// The coordinator must retain sole task-set ownership: prevent new ordinary
/// tasks, include every non-exempt task, and keep RTC/recovery infrastructure
/// outside the set. Existing tasks must eventually call [`checkpoint`] without
/// locks/resources needed by the coordinator. Tasks that cannot do so cause a
/// timeout, never a false acknowledgment. Exit is acknowledged only after switch-out.
/// IRQs and logical time remain running throughout the wait. The set may be
/// empty when the caller has independently excluded all ordinary execution.
///
/// # Errors
/// Admission rejects invalid context/targets before publishing requests.
/// Cancellation or timeout removes all installed requests and wakes acknowledged
/// tasks before returning. No power or timekeeping transition is performed.
pub fn freeze_tasks(
    tasks: &[KtaskRef],
    timeout: TimeSpan,
    cancellation: &FreezeCancellation,
) -> Result<FrozenTasks, FreezeError> {
    if !crate::is_sleepable_context() {
        return Err(FreezeError::InvalidContext);
    }
    let current = crate::current();
    for (index, task) in tasks.iter().enumerate() {
        if KtaskRef::ptr_eq(task, &current.clone())
            || task.is_idle()
            || tasks[..index]
                .iter()
                .any(|other| KtaskRef::ptr_eq(other, task))
        {
            return Err(FreezeError::InvalidTaskSet);
        }
    }
    let admission = Admission::acquire()?;
    let wait = CompletionWait::prepare().map_err(|_| FreezeError::InvalidContext)?;
    let deadline = khal::time::monotonic_time() + timeout;
    let progress = cancellation.0.clone();
    let timer = crate::Timer::new(move |_: crate::TimerControl<'_>, _: crate::Expiration| {
        progress.changed.notify_all();
    });
    // SAFETY: admission validated initialized sleepable task context. The owned
    // handler only performs allocation-free completion notification; it never
    // sleeps, enables IRQs, cancels itself or retains borrowed state. Timer Drop
    // drains delivery in this task before any PM caller can freeze time/events.
    unsafe { timer.start_if_future(deadline) }.map_err(FreezeError::Timer)?;
    let round = Arc::new(Round {
        progress: cancellation.0.clone(),
        is_released: AtomicBool::new(false),
    });
    let requests = tasks
        .iter()
        .map(|task| {
            (
                task.clone(),
                Arc::new(Request {
                    round: round.clone(),
                    is_confirmed: AtomicBool::new(false),
                    has_entered: AtomicBool::new(false),
                }),
            )
        })
        .collect();
    let frozen = FrozenTasks {
        requests,
        round,
        _task_bound: PhantomData,
        _admission: admission,
    };
    for (task, request) in &frozen.requests {
        *task.freeze_request.lock() = Some(request.clone());
        // Release the request lock before scheduler entry. A freezer-aware
        // park rechecks the request after publishing Blocked, closing the
        // running-to-blocked race even when this wake observes a running task.
        if task.is_freezer_wait.load(Ordering::Acquire) {
            crate::wake_task(task, true);
        }
        crate::run_queue::kick_running_task(task);
    }
    cancellation.0.changed.notify_all();
    let confirmed = || {
        frozen.requests.iter().all(|(task, request)| {
            request.is_confirmed.load(Ordering::Acquire)
                || task.is_exit_complete.load(Ordering::Acquire)
        })
    };
    wait.wait_until(&cancellation.0.changed, || {
        cancellation.0.is_cancelled.load(Ordering::Acquire)
            || confirmed()
            || khal::time::monotonic_time() >= deadline
    });
    if cancellation.0.is_cancelled.load(Ordering::Acquire) {
        return Err(FreezeError::Cancelled);
    }
    if !confirmed() {
        for (task, request) in &frozen.requests {
            if !request.is_confirmed.load(Ordering::Acquire)
                && !task.is_exit_complete.load(Ordering::Acquire)
            {
                let mut name = [0u8; 96];
                let length = task.copy_name(&mut name);
                let name = core::str::from_utf8(&name[..length]).unwrap_or("<invalid task name>");
                log::warn!(
                    "task freeze missing checkpoint: name={name} state={:?}",
                    task.state()
                );
            }
        }
        return Err(FreezeError::TimedOut);
    }
    Ok(frozen)
}

/// Confirms the current task's request at an explicitly audited safe point.
///
/// Call with no locks, RCU read scopes, device operations or resources needed by
/// PM/recovery. May block until thaw. Ordinary wakeups, signals and cancellation
/// do not bypass the persistent release predicate. No signal/EINTR is generated.
/// User-return and first-task-entry paths call this automatically; kernel workers
/// must opt in at safe loop boundaries. A syscall stuck before its next safe
/// point causes the coordinator to time out.
///
/// # Errors
/// Returns InvalidContext if a request is pending but blocking is not legal;
/// that task remains unconfirmed and PM cannot proceed on its behalf.
pub fn checkpoint() -> Result<(), FreezeError> {
    if !ACTIVE.load(Ordering::Acquire) {
        return Ok(());
    }
    if !crate::is_sleepable_context() {
        return Err(FreezeError::InvalidContext);
    }
    let task = crate::current().clone();
    let request = task.freeze_request.lock().clone();
    let Some(request) = request else {
        return Ok(());
    };
    // Only the freezer release path can wake Frozen. Ordinary notifications
    // keep their own wake latches, without executing this stack while held.
    request.has_entered.store(true, Ordering::Release);
    let woke = crate::park::ParkLatch::new();
    while !request.round.is_released.load(Ordering::Acquire) {
        crate::park::park_task_unless(
            &woke,
            TaskState::Frozen,
            || request.round.is_released.load(Ordering::Acquire),
            || {},
        );
    }
    Ok(())
}

/// Captures the outgoing request before clearing on_cpu permits remote enqueue.
/// A late completion must never acknowledge a newer request on the same task.
pub(crate) struct SwitchCompletion {
    is_exit: bool,
    request: Option<Arc<Request>>,
}

impl SwitchCompletion {
    pub(crate) fn prepare(task: &KtaskRef) -> Self {
        let state = task.state();
        let is_exit = state == TaskState::Exited;
        let request = if ACTIVE.load(Ordering::Acquire) && (is_exit || state == TaskState::Frozen) {
            task.freeze_request
                .lock()
                .clone()
                .filter(|request| is_exit || request.has_entered.load(Ordering::Acquire))
        } else {
            None
        };
        Self { is_exit, request }
    }

    /// Runs on the incoming stack after terminal cleanup and the wake handoff.
    /// No scheduler lock may be held: notification can wake a remote coordinator.
    pub(crate) fn complete(self, task: &KtaskRef) {
        if self.is_exit {
            task.is_exit_complete.store(true, Ordering::Release);
            system::retire(task);
        }
        // Exit cannot resume: a request published after prepare still needs
        // notification when the completed-exit predicate becomes visible.
        let request = if self.is_exit {
            task.freeze_request.lock().clone()
        } else {
            self.request
        };
        if let Some(request) = request {
            if !self.is_exit {
                request.is_confirmed.store(true, Ordering::Release);
            }
            request.round.progress.changed.notify_all();
        }
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/freezer.rs");

#[cfg(unittest)]
unittest::path_mod_test!(wait_tests, "../ut-tests/freezer_wait.rs");

#[cfg(unittest)]
unittest::path_mod_test!(/// Test-only scheduler admission fixtures for serial integration cases.
pub mod test_support, "../ut-tests/freezer/test_support.rs");
