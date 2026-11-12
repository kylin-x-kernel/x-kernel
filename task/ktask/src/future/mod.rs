// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Future support.

use alloc::{sync::Arc, task::Wake};
use core::{
    fmt,
    future::poll_fn,
    marker::PhantomData,
    pin::pin,
    task::{Context, Poll, Waker},
};

use kerrno::{KError, KResult};
use kpoll::{PollRegisterError, PollRegistrations};
use kspin::{NoPreemptIrqSave, SpinNoIrq};

use crate::{KtaskRef, TaskState, WeakKtaskRef, current, park::park_task, select_wake_run_queue};

mod poll;
pub use poll::*;

mod time;
pub use time::*;

struct KWaker {
    task: WeakKtaskRef,
    woke: SpinNoIrq<bool>,
}

impl KWaker {
    fn new(task: &KtaskRef) -> Arc<Self> {
        Arc::new(KWaker {
            task: KtaskRef::downgrade(task),
            woke: SpinNoIrq::new(false),
        })
    }

    // The caller retains a strong task reference across this handshake.
    fn park(&self) {
        park_task(&self.woke);
    }
}

impl Wake for KWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(task) = self.task.upgrade() {
            {
                let mut woke = self.woke.lock();
                *woke = true;
            }
            select_wake_run_queue::<NoPreemptIrqSave>(&task).unblock_task(task, true);
        }
    }
}

/// Task-bound parking resources prepared before registering a wake source.
///
/// This handle is neither Send nor Sync. Only the preparing task may park;
/// the independently owned [`Waker`] may be notified from IRQ context. Wakeups
/// are hints, not completion results: callers must recheck their predicate.
pub struct PreparedTaskWait {
    task: KtaskRef,
    kwaker: Arc<KWaker>,
    _task_bound: PhantomData<*mut ()>,
}

impl PreparedTaskWait {
    /// Validates the current context and allocates one reusable waker.
    ///
    /// Call with IRQs enabled and without any lock that forbids sleeping.
    /// The allocation follows the kernel's infallible Arc OOM policy.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] before allocation if no non-idle task
    /// exists, IRQs are masked, interrupt-like context is active, or preemption
    /// is disabled where that depth is tracked.
    pub fn prepare() -> KResult<Self> {
        let curr = crate::current_may_uninit().ok_or(KError::InvalidInput)?;
        if !crate::is_sleepable_context() {
            return Err(KError::InvalidInput);
        }
        let task = curr.clone();
        let kwaker = KWaker::new(&task);
        Ok(Self {
            task,
            kwaker,
            _task_bound: PhantomData,
        })
    }

    /// Clones the prepared waker without allocating or registering a wait.
    ///
    /// It holds only a weak task reference and may outlive this handle.
    pub fn waker(&self) -> Waker {
        Waker::from(self.kwaker.clone())
    }

    /// Parks until a wake hint, consuming a prior hint without blocking.
    ///
    /// May return spuriously. No wait registration, waker allocation or signal
    /// interruption is performed. Callers recheck their completion predicate
    /// after every return and keep their wake source registered while needed.
    ///
    /// # Panics
    /// Panics if called by another task or outside sleepable context. The
    /// preparing task must release all locks that forbid sleeping before park.
    pub fn park(&mut self) {
        self.validate_park();
        self.kwaker.park();
    }

    /// Parks a kernel worker at an explicitly audited freezer safe point.
    ///
    /// Unlike ordinary parking, a freeze request can wake this wait without an
    /// external work notification. Requests are confirmed before parking and
    /// after wake; a confirmed task cannot return until thaw. Wake hints arriving
    /// while frozen remain latched. Callers must recheck work/stop predicates.
    /// No signals are consumed and no EINTR is generated.
    ///
    /// Requires a kernel task with no locks, RCU scopes, callback payload or
    /// device operations needed by PM/recovery retained across this call. This
    /// is opt-in: ordinary waits and blocked callbacks never acknowledge freezes.
    ///
    /// # Errors
    /// Returns InvalidContext for user tasks or non-sleepable context, before
    /// opting into wakeup or acknowledging a request.
    ///
    /// # Panics
    /// Panics if called by a task other than the preparing task.
    pub fn park_freezable(&mut self) -> Result<(), crate::freezer::FreezeError> {
        if !crate::is_sleepable_context() || self.task.user_runtime().is_some() {
            return Err(crate::freezer::FreezeError::InvalidContext);
        }
        self.validate_park();
        let _registration = FreezerWait::new(&self.task.is_freezer_wait);
        crate::freezer::checkpoint()?;
        crate::park::park_task_unless(
            &self.kwaker.woke,
            TaskState::Blocked,
            || self.task.freeze_request.lock().is_some(),
            || {},
        );
        crate::freezer::checkpoint()
    }

    fn validate_park(&self) {
        let curr = current();
        assert!(
            curr.ptr_eq(&self.task),
            "prepared wait belongs to another task"
        );
        assert!(
            crate::is_sleepable_context(),
            "prepared wait cannot park in this context"
        );
    }
}

struct FreezerWait<'a>(&'a core::sync::atomic::AtomicBool);

impl<'a> FreezerWait<'a> {
    fn new(active: &'a core::sync::atomic::AtomicBool) -> Self {
        active.store(true, core::sync::atomic::Ordering::Release);
        Self(active)
    }
}

impl Drop for FreezerWait<'_> {
    fn drop(&mut self) {
        self.0.store(false, core::sync::atomic::Ordering::Release);
    }
}

/// Blocks the current task until the given future is resolved.
///
/// Note that this doesn't handle interruption and is not recommended for direct
/// use in most cases.
pub fn block_on<F: IntoFuture>(f: F) -> F::Output {
    block_on_with_state(f, TaskState::Blocked)
}

/// Runs a future with an interruptible task sleep state.
///
/// Like [`interruptible`], operation progress takes precedence over pending
/// interruption. A signal wake can make the task runnable directly; the
/// operation's poll result still decides whether to return a value or an error.
/// The task's interrupt state is rechecked after publishing its sleeping state
/// so an interrupt racing the transition cannot be lost.
/// Call in sleepable task context without locks that prohibit blocking.
///
/// # Errors
/// Returns [`Interrupted`] when an interrupt is consumed while the operation
/// cannot progress, or when registering its interrupt notification fails.
pub fn block_on_interruptible<F: IntoFuture>(f: F) -> Result<F::Output, Interrupted> {
    block_on_with_state(interruptible(f), TaskState::BlockedInterruptible)
}

fn block_on_with_state<F: IntoFuture>(f: F, state: TaskState) -> F::Output {
    let mut fut = pin!(f.into_future());

    let curr = current();
    // Caller-owned strong ref for `blocked_resched`: current slot +
    // Scheduler.curr + this clone (also seeds KWaker's weak upgrade).
    let task = curr.clone();

    let kwaker = KWaker::new(&task);
    let waker = Waker::from(kwaker.clone());
    let mut cx = Context::from_waker(&waker);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => {
                crate::park::park_task_unless(
                    &kwaker.woke,
                    state,
                    || state == TaskState::BlockedInterruptible && task.is_interrupted(),
                    || {},
                );
            }
            Poll::Ready(output) => break output,
        }
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(prepared_tests, "../../ut-tests/future/prepared_tests.rs");

/// Error returned by [`interruptible`].
#[derive(Debug, PartialEq, Eq)]
pub struct Interrupted(InterruptCause);

#[derive(Debug, PartialEq, Eq)]
enum InterruptCause {
    Task,
    Registration(PollRegisterError),
}

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            InterruptCause::Task => write!(f, "interrupted"),
            InterruptCause::Registration(error) => write!(f, "{error}"),
        }
    }
}

impl Interrupted {
    /// Returns true when the wait was interrupted by [`crate::interrupt_task`].
    ///
    /// The wakeup need not be a POSIX signal: virtual IRQ injection and other
    /// asynchronous events use the same task-interrupt path.
    pub fn is_task_interrupt(&self) -> bool {
        matches!(self.0, InterruptCause::Task)
    }
}

impl core::error::Error for Interrupted {}

impl From<Interrupted> for KError {
    fn from(error: Interrupted) -> Self {
        match error.0 {
            InterruptCause::Task => KError::Interrupted,
            InterruptCause::Registration(
                PollRegisterError::NoMemory | PollRegisterError::IdExhausted,
            ) => KError::NoMemory,
            InterruptCause::Registration(PollRegisterError::InvalidState) => KError::InvalidInput,
        }
    }
}

/// Makes a future interruptible.
///
/// Operation progress has priority over signal delivery. The wrapped future is
/// polled first, and its value is returned whenever it is ready; a pending
/// interrupt is left pending so it can interrupt a later wait. The interrupt
/// state is consumed only after the operation reports [`Poll::Pending`], at
/// which point a signal makes the wait fail with [`Interrupted`].
///
/// Poll this future in task context without locks that prohibit blocking.
/// Dropping it removes all wait registrations it retained.
pub async fn interruptible<F: IntoFuture>(f: F) -> Result<F::Output, Interrupted> {
    let mut f = pin!(f.into_future());
    let curr = current();
    let mut registrations = PollRegistrations::new();
    poll_fn(|cx| {
        if let Poll::Ready(value) = f.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }

        // The operation cannot make progress in this round. Observe a pending
        // task interrupt (normally a signal) now, and keep both its
        // registration and the operation's registrations alive until the next
        // poll.
        let mut context = registrations.context(cx);
        match curr.poll_interrupt(&mut context) {
            Ok(Poll::Ready(())) => Poll::Ready(Err(Interrupted(InterruptCause::Task))),
            Ok(Poll::Pending) => Poll::Pending,
            Err(error) => Poll::Ready(Err(Interrupted(InterruptCause::Registration(error)))),
        }
    })
    .await
}
