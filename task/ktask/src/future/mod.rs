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

use crate::{KtaskRef, WeakKtaskRef, current, current_run_queue, select_wake_run_queue};

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
            task: Arc::downgrade(task),
            woke: SpinNoIrq::new(false),
        })
    }

    // The caller retains a strong task reference across this handshake.
    fn park(&self) {
        {
            let mut woke = self.woke.lock();
            if *woke {
                *woke = false;
                return;
            }
        }

        let mut rq = current_run_queue::<NoPreemptIrqSave>();
        let mut woke = self.woke.lock();
        if *woke {
            *woke = false;
            return;
        }

        // Publish Blocked before dropping the latch lock, so a concurrent
        // wake either prevents parking or observes a task it can unblock.
        rq.blocked_resched(woke);
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

/// Task-bound parking resources prepared before publishing an operation.
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
        let curr = current();
        assert!(
            curr.ptr_eq(&self.task),
            "prepared wait belongs to another task"
        );
        assert!(
            crate::is_sleepable_context(),
            "prepared wait cannot park in this context"
        );
        self.kwaker.park();
    }
}

/// Blocks the current task until the given future is resolved.
///
/// Note that this doesn't handle interruption and is not recommended for direct
/// use in most cases.
pub fn block_on<F: IntoFuture>(f: F) -> F::Output {
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
            Poll::Pending => kwaker.park(),
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
    Signal,
    Registration(PollRegisterError),
}

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            InterruptCause::Signal => write!(f, "interrupted"),
            InterruptCause::Registration(error) => write!(f, "{error}"),
        }
    }
}

impl Interrupted {
    /// Returns true when the wait was interrupted by [`crate::interrupt_task`].
    pub fn is_signal(&self) -> bool {
        matches!(self.0, InterruptCause::Signal)
    }
}

impl core::error::Error for Interrupted {}

impl From<Interrupted> for KError {
    fn from(error: Interrupted) -> Self {
        match error.0 {
            InterruptCause::Signal => KError::Interrupted,
            InterruptCause::Registration(
                PollRegisterError::NoMemory | PollRegisterError::IdExhausted,
            ) => KError::NoMemory,
            InterruptCause::Registration(PollRegisterError::InvalidState) => KError::InvalidInput,
        }
    }
}

/// Makes a future interruptible.
pub async fn interruptible<F: IntoFuture>(f: F) -> Result<F::Output, Interrupted> {
    let mut f = pin!(f.into_future());
    let curr = current();
    let mut registrations = PollRegistrations::new();
    poll_fn(|cx| {
        let mut context = registrations.context(cx);
        match curr.poll_interrupt(&mut context) {
            Ok(Poll::Ready(())) => return Poll::Ready(Err(Interrupted(InterruptCause::Signal))),
            Ok(Poll::Pending) => {}
            Err(error) => {
                return Poll::Ready(Err(Interrupted(InterruptCause::Registration(error))));
            }
        }
        drop(context);
        f.as_mut().poll(cx).map(Ok)
    })
    .await
}
