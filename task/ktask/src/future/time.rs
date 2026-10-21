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

/// Waits for an absolute BOOTTIME deadline, including injected sleep time.
///
/// Poll in task context. Clock registration errors are returned to the caller.
/// The same IRQ-waker and cancellation constraints as [`sleep_until`] apply.
///
/// # Errors
/// Forwards poll registration errors and timer-provider start errors. The
/// deadline is clamped, so a provider InvalidInput indicates CPU routing/setup
/// inconsistency rather than an out-of-range user deadline.
pub async fn sleep_until_boottime(deadline: ktime_types::BoottimeInstant) -> Result<(), KError> {
    sleep_until_clock(
        ktime::ClockDomain::Boottime,
        || ktime::boottime() >= deadline,
        || ktime::boottime_deadline_to_monotonic(deadline),
    )
    .await
}

/// Waits for an absolute REALTIME deadline, tracking wall-clock changes.
///
/// Relative REALTIME waits must use [`sleep`] instead. Poll in task context;
/// registration errors and the cancellation constraints of [`sleep_until`] apply.
///
/// # Errors
/// Forwards poll registration and timer-provider errors, as described by
/// [`sleep_until_boottime`].
pub async fn sleep_until_realtime(deadline: ktime_types::SystemTime) -> Result<(), KError> {
    sleep_until_clock(
        ktime::ClockDomain::Realtime,
        || ktime::realtime() >= deadline,
        || ktime::realtime_deadline_to_monotonic(deadline),
    )
    .await
}

async fn sleep_until_clock(
    clock: ktime::ClockDomain,
    is_due: impl Fn() -> bool,
    timer_deadline: impl Fn() -> MonotonicInstant,
) -> Result<(), KError> {
    loop {
        // Capture generation before the clock snapshot, so an update during
        // timer preparation cannot strand this waiter on a stale deadline.
        let mut change = ktime::clock_change_listener(clock);
        if is_due() {
            return Ok(());
        }
        let horizon = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
        let mut timer = TimerFuture::new(timer_deadline().min(horizon))?;
        let is_complete = core::future::poll_fn(|cx| {
            // Every poll checks the original domain, including spurious wakes
            // and changes that arrive before the notification is polled.
            if is_due() {
                return Poll::Ready(Ok(true));
            }
            match Pin::new(&mut change).poll(cx) {
                Poll::Ready(Ok(())) => return Poll::Ready(Ok(false)),
                Poll::Ready(Err(error)) => {
                    return Poll::Ready(Err(super::poll::map_register_error(error)));
                }
                Poll::Pending => {}
            }
            if Pin::new(&mut timer).poll(cx).is_ready() {
                Poll::Ready(Ok(false))
            } else {
                Poll::Pending
            }
        })
        .await?;
        if is_complete {
            return Ok(());
        }
    }
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
