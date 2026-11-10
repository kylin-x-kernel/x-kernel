// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Waiting for fences from task context.
//!
//! This is the one place a syscall parks on a completion point. It mirrors
//! `dma_fence_wait_timeout`:
//!
//! * a set that already completed returns its outcome without sleeping;
//! * otherwise the task parks until a fence of the set completes, and
//!   re-evaluates — a wake is a hint, never the answer itself;
//! * an error any fence finished with is returned as that error, so a failed
//!   producer never looks like a completed one;
//! * a signal ends an interruptible wait with [`DrmError::Interrupted`];
//! * a deadline ends it with [`DrmError::TimedOut`]. The deadline is the
//!   caller's, plus the one a producer published with
//!   [`DmaFence::set_deadline`]: a fence that carries its own deadline bounds
//!   every wait on it, exactly as a driver hints at its queue's progress.
//!
//! The wait is built on [`ktask::future::block_on_interruptible`] and
//! [`Wait`](crate::sync::dma_fence::Wait): the fence's completion sets the
//! waiter's waker, which ends the parked future.

use core::{
    future::{Future, IntoFuture, poll_fn},
    pin::pin,
    task::Poll,
};

use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ktask::{Expiration, Timer, TimerControl, TimerHandler, future::block_on_interruptible};
use ktime_types::MonotonicInstant;

use super::{
    dma_fence::Wait,
    fence_list::{FenceList, first_error, is_signaled},
};
use crate::{model::error::DrmError, prelude::*};

/// Waits until every fence of `fences` completed successfully.
///
/// # Errors
///
/// Returns the first fence error, [`DrmError::TimedOut`] when a deadline of
/// the set expires, or [`DrmError::Interrupted`] when a signal ends the wait.
pub(crate) fn wait_for_fences(fences: &Arc<FenceList>) -> DrmResult<()> {
    if is_signaled(fences) {
        return Ok(());
    }
    let deadline = fences
        .fences()
        .iter()
        .filter_map(|fence| fence.deadline())
        .min();
    let wait = Wait::new();
    match block_on_with_deadline(
        poll_fn(|cx| match wait.outcome(fences, cx.waker()) {
            Some(0) => Poll::Ready(Ok(())),
            Some(error) => Poll::Ready(Err(DrmError::from_fence(error))),
            None => Poll::Pending,
        }),
        deadline,
    ) {
        // A signal ends the wait unless the set completed while it ran.
        Err(_interrupted) => {
            if is_signaled(fences) {
                Ok(())
            } else {
                Err(DrmError::Interrupted)
            }
        }
        Ok(result) => result,
    }
}

/// Runs `future` until it completes, a signal ends it, or `deadline` passes.
///
/// A deadline is enforced by one sleeping timer that rouses the task: the
/// future re-polls, and the deadline that has passed turns into
/// [`DrmError::TimedOut`]. Without a deadline this is the runtime's
/// interruptible park and nothing else.
///
/// # Errors
///
/// Returns the runtime's `Interrupted` for a signal, and [`DrmError::TimedOut`]
/// as the future's output type when the deadline expired; the future itself
/// never observes the timeout.
pub(crate) fn block_on_with_deadline<F>(
    future: F,
    deadline: Option<MonotonicInstant>,
) -> Result<F::Output, ktask::future::Interrupted>
where
    F: IntoFuture<Output = DrmResult<()>>,
{
    let Some(at) = deadline else {
        return block_on_interruptible(future);
    };
    let mut timer = Deadline::new(at);
    let mut inner = pin!(future.into_future());
    block_on_interruptible(poll_fn(|cx| {
        if monotonic_time() >= at {
            return Poll::Ready(Err(DrmError::TimedOut));
        }
        match inner.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                timer.arm(cx.waker());
                Poll::Pending
            }
        }
    }))
}

/// The timer of one wait that must end at a known instant.
///
/// The timer exists only so a fence whose producer stopped reporting cannot
/// park the task forever; it is created once per wait and re-armed from every
/// poll. When the deadline has already passed, the task is roused at once
/// instead of sleeping until a timer that fires immediately.
struct Deadline {
    at: MonotonicInstant,
    timer: Timer<DeadlineWake>,
}

impl Deadline {
    fn new(at: MonotonicInstant) -> Self {
        Self {
            at,
            timer: Timer::new(DeadlineWake {
                waker: SpinNoIrq::new(None),
            }),
        }
    }

    /// Makes the timer wake the parked task at the deadline.
    fn arm(&mut self, waker: &core::task::Waker) {
        *self.timer.handler().waker.lock() = Some(waker.clone());
        if monotonic_time() >= self.at {
            rouse(&self.timer);
            return;
        }
        // SAFETY: the handler only takes a waker and wakes the task, so it
        // neither sleeps nor re-enters timer dispatch. Every fence wait runs in
        // sleepable syscall context on an initialized CPU, which satisfies
        // `Timer::start`'s execution-context contract.
        if unsafe { self.timer.start(self.at) }.is_err() {
            // A timer that cannot be armed must not strand the task: it is
            // woken at once and re-evaluates against the clock.
            rouse(&self.timer);
        }
    }
}

/// Wakes the task parked behind `timer`, if any.
fn rouse(timer: &Timer<DeadlineWake>) {
    let waker = timer.handler().waker.lock().take();
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Timer handler that rouses the parked wait.
struct DeadlineWake {
    waker: SpinNoIrq<Option<core::task::Waker>>,
}

impl TimerHandler for DeadlineWake {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        // The waker may take scheduler locks, so it is taken out first and
        // called after the guard is released.
        let waker = self.waker.lock().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Reports whether any fence of `fences` failed, without waiting.
///
/// # Errors
///
/// Returns the first fence error.
pub(crate) fn check_error(fences: &FenceList) -> DrmResult<()> {
    match first_error(fences) {
        0 => Ok(()),
        error => Err(DrmError::from_fence(error)),
    }
}
