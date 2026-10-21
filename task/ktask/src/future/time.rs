// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned async sleep timers and operation-first timeouts.

use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use futures_util::{FutureExt, select_biased};
use kerrno::KError;
use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

struct SleepState {
    is_expired: bool,
    waker: Option<Waker>,
}

impl SleepState {
    fn complete(&mut self) -> Option<Waker> {
        self.is_expired = true;
        self.waker.take()
    }
}

struct SleepHandler(SpinNoIrq<SleepState>);

impl crate::TimerHandler for SleepHandler {
    fn on_expiration(&self, _: crate::TimerControl<'_>, _: crate::Expiration) {
        let waker = self.0.lock().complete();
        // Waker code may take task/runqueue locks: never call it under our lock.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Future returned by sleep utilities, owning its prepared timer.
///
/// Dropping it synchronously cancels delivery. Do not drop it from its own
/// wake callback or while preventing the callback's CPU from progressing.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct TimerFuture {
    // Timer Drop drains delivery before releasing its embedded wake state.
    timer: crate::Timer<SleepHandler>,
}

impl TimerFuture {
    fn new(deadline: MonotonicInstant) -> Result<Self, KError> {
        let timer = crate::Timer::new(SleepHandler(SpinNoIrq::new(SleepState {
            is_expired: false,
            waker: None,
        })));
        // SAFETY: task sleep runs after local scheduler/IRQ setup. The handler
        // only publishes completion and wakes the task outside its state lock,
        // matching the existing IRQ-delivered task-waker contract.
        if !unsafe { timer.start_if_future(deadline) }? {
            timer.handler().0.lock().is_expired = true;
        }
        Ok(Self { timer })
    }
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Clone/drop arbitrary waker implementations outside the state lock.
        let replacement = cx.waker().clone();
        let mut state = self.timer.handler().0.lock();
        if state.is_expired {
            drop(state);
            return Poll::Ready(());
        }
        let previous = state.waker.replace(replacement);
        drop(state);
        drop(previous);
        Poll::Pending
    }
}

/// Waits until duration has elapsed.
///
/// # Panics
/// Panics if adding duration overflows the monotonic time representation.
pub async fn sleep(duration: TimeSpan) {
    sleep_until(monotonic_time() + duration).await
}

/// Waits until deadline is reached.
///
/// Deadlines beyond the timer's u64-nanosecond runtime horizon remain pending
/// throughout that supported runtime, rather than failing a long syscall wait.
/// A future deadline prepares one owned allocation on the first poll. The
/// executor's waker must support hard-IRQ wake without sleeping or unwinding.
/// Dropping a pending wait drains its callback; do not drop from its own wake
/// callback or while holding a lock that callback needs.
///
/// # Panics
/// Panics if the initialized CPU/service invariant is violated during start
/// or notification. Allocation failure follows the kernel allocator policy.
pub async fn sleep_until(deadline: MonotonicInstant) {
    if deadline <= monotonic_time() {
        return;
    }
    // TimeSpan supports longer syscall waits than the core runtime horizon.
    // The horizon is a valid future deadline, never an inactive sentinel; it
    // cannot wake this wait early while the supported clock is below u64::MAX.
    let horizon = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
    TimerFuture::new(deadline.min(horizon))
        .expect("bounded sleep deadline and current CPU are valid")
        .await;
}

/// Error returned by [`timeout`] and [`timeout_at`].
#[derive(Debug, PartialEq, Eq)]
pub struct Elapsed(());

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "deadline elapsed")
    }
}

impl core::error::Error for Elapsed {}

impl From<Elapsed> for KError {
    fn from(_: Elapsed) -> Self {
        KError::TimedOut
    }
}

/// Requires a `Future` to complete before the specified duration has elapsed.
pub async fn timeout<F: IntoFuture>(
    duration: Option<TimeSpan>,
    f: F,
) -> Result<F::Output, Elapsed> {
    timeout_at(
        duration.and_then(|duration| khal::time::monotonic_time().checked_add(duration)),
        f,
    )
    .await
}

/// Requires a `Future` to complete before the specified deadline.
pub async fn timeout_at<F: IntoFuture>(
    deadline: Option<MonotonicInstant>,
    f: F,
) -> Result<F::Output, Elapsed> {
    if let Some(deadline) = deadline {
        select_biased! {
            res = f.into_future().fuse() => Ok(res),
            _ = sleep_until(deadline).fuse() => Err(Elapsed(())),
        }
    } else {
        Ok(f.await)
    }
}

unittest::path_mod_test!(
    owned_sleep_tests,
    "../../ut-tests/future/time/owned_sleep_tests.rs"
);

unittest::path_mod_test!(
    tests_subtick_precision,
    "../../ut-tests/future/time/tests_subtick_precision.rs"
);
