// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Future support.

use alloc::{sync::Arc, task::Wake};
use core::{
    fmt,
    future::poll_fn,
    pin::pin,
    task::{Context, Poll, Waker},
};

use kerrno::KError;
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

/// Blocks the current task until the given future is resolved.
///
/// Note that this doesn't dispatch_irq interruption and is not recommended for direct
/// use in most cases.
pub fn block_on<F: IntoFuture>(f: F) -> F::Output {
    let mut fut = pin!(f.into_future());

    let curr = current();
    // Caller-owned strong ref for `blocked_resched`: into_raw current = 1;
    // this clone is the required second (also seeds KWaker's weak upgrade).
    let task = curr.clone();

    let kwaker = KWaker::new(&task);
    let waker = Waker::from(kwaker.clone());
    let mut cx = Context::from_waker(&waker);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => {
                let mut rq = current_run_queue::<NoPreemptIrqSave>();
                let mut woke = kwaker.woke.lock();
                if !*woke {
                    // blocked_resched() will set *woke = false and drop
                    // the guard internally before rescheduling. When this
                    // task is woken, woke will be set to true by the waker
                    // and we'll re-enter the loop to poll again.
                    rq.blocked_resched(woke);
                } else {
                    // The waker fired after `poll()` returned Pending but before
                    // this task committed to blocking. Re-poll immediately:
                    // yielding here can hand a saturated CPU to an unrelated
                    // runnable task even though this future is already ready,
                    // turning a resolved wake-before-block race into scheduler
                    // latency. If the wake was spurious, the next Pending poll
                    // will take the normal blocking path.
                    *woke = false;
                    drop(woke);
                }
            }
            Poll::Ready(output) => break output,
        }
    }
}

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
