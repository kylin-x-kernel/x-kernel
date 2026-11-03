// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cooperative freezing of an explicit, stable task set on one present CPU.
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
pub use system::{SystemFrozenTasks, freeze_system_tasks};

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
    /// Remote CPU rendezvous is not implemented.
    MultipleCpus,
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
    // Release/acquire pairs thaw with checkpoint waiters, including a waiter
    // preempted between acknowledgment and actual scheduler blocking.
    is_released: AtomicBool,
    thaw: kcompletion::WaitQueue,
}

pub(crate) struct Request {
    round: Arc<Round>,
    // Acquire in the owner observes the task's release at a safe checkpoint.
    is_confirmed: AtomicBool,
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
        self.round.thaw.notify_all();
    }
}

/// Requests and confirms a stable set of tasks before its owner may suspend.
///
/// The coordinator must retain sole task-set ownership: prevent new ordinary
/// tasks, include every non-exempt task, and keep RTC/recovery infrastructure
/// outside the set. Existing tasks must eventually call [`checkpoint`] without
/// locks/resources needed by the coordinator. Tasks that cannot do so cause a
/// timeout, never a false acknowledgment. Exited tasks are already quiescent.
/// IRQs and logical time remain running throughout the wait. The set may be
/// empty when the caller has independently excluded all ordinary execution.
///
/// # Errors
/// Admission rejects invalid context/topology/targets before publishing requests.
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
    if kcpu_id_map::nr_cpus() != 1 {
        return Err(FreezeError::MultipleCpus);
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
        thaw: kcompletion::WaitQueue::new(),
    });
    let requests = tasks
        .iter()
        .map(|task| {
            (
                task.clone(),
                Arc::new(Request {
                    round: round.clone(),
                    is_confirmed: AtomicBool::new(false),
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
    }
    cancellation.0.changed.notify_all();
    let confirmed = || {
        frozen.requests.iter().all(|(task, request)| {
            request.is_confirmed.load(Ordering::Acquire) || task.state() == TaskState::Exited
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
    let request = crate::current().freeze_request.lock().clone();
    let Some(request) = request else {
        return Ok(());
    };
    let wait = CompletionWait::prepare().map_err(|_| FreezeError::InvalidContext)?;
    request.is_confirmed.store(true, Ordering::Release);
    request.round.progress.changed.notify_all();
    wait.wait_until(&request.round.thaw, || {
        request.round.is_released.load(Ordering::Acquire)
    });
    Ok(())
}

pub(crate) fn notify_exit(task: &crate::TaskInner) {
    let request = task.freeze_request.lock().clone();
    if let Some(request) = request {
        request.round.progress.changed.notify_all();
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/freezer.rs");
