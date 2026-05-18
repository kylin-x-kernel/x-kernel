// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async time utilities and timer wheel integration.

use alloc::collections::BTreeMap;
use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures_util::{FutureExt, select_biased};
use kcpu_id_map::LogicalCpuId;
use kerrno::KError;
use khal::time::{TimeValue, monotonic_time};
use kspin::SpinNoIrq;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TimerKey {
    deadline: TimeValue,
    key: u64,
}

#[derive(Debug)]
pub struct TimerHandle {
    key: TimerKey,
    cpu_id: LogicalCpuId,
}

struct TimerRuntime {
    key: u64,
    wheel: BTreeMap<TimerKey, Waker>,
}

impl TimerRuntime {
    const fn new() -> Self {
        TimerRuntime {
            key: 0,
            wheel: BTreeMap::new(),
        }
    }

    fn add(&mut self, deadline: TimeValue) -> Option<TimerKey> {
        if deadline <= monotonic_time() {
            return None;
        }

        let key = TimerKey {
            deadline,
            key: self.key,
        };
        self.wheel.insert(key, Waker::noop().clone());
        self.key += 1;

        Some(key)
    }

    fn poll(&mut self, key: &TimerKey, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(w) = self.wheel.get_mut(key) {
            *w = cx.waker().clone();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn add_with_waker(&mut self, deadline: TimeValue, waker: Waker) -> Option<TimerKey> {
        if deadline <= monotonic_time() {
            return None;
        }
        let key = TimerKey {
            deadline,
            key: self.key,
        };
        self.wheel.insert(key, waker);
        self.key += 1;
        Some(key)
    }

    fn cancel(&mut self, key: &TimerKey) {
        self.wheel.remove(key);
    }

    fn take_expired_wakers(&mut self) -> BTreeMap<TimerKey, Waker> {
        if self.wheel.is_empty() {
            return BTreeMap::new();
        }

        let now = monotonic_time();

        let pending = self.wheel.split_off(&TimerKey {
            deadline: now,
            key: u64::MAX,
        });

        core::mem::replace(&mut self.wheel, pending)
    }
}

percpu_static! {
    TIMER_RUNTIME: SpinNoIrq<TimerRuntime> = SpinNoIrq::new(TimerRuntime::new()),
}

fn timer_runtime(cpu_id: LogicalCpuId) -> &'static SpinNoIrq<TimerRuntime> {
    // SAFETY:
    // 1. `cpu_id` is either returned by `this_cpu_id()` or stored from a
    //    previously returned value, so it identifies an initialized CPU.
    // 2. `TIMER_RUNTIME` is a `SpinNoIrq<TimerRuntime>`, and all mutable
    //    accesses to the timer wheel go through the lock.
    unsafe { TIMER_RUNTIME.remote_ref_raw(cpu_id.as_usize()) }
}

#[allow(dead_code)]
pub(crate) fn check_timer_events() {
    let cpu_id = khal::percpu::this_cpu_id();
    let wakers = timer_runtime(cpu_id).lock().take_expired_wakers();
    for (_, waker) in wakers {
        waker.wake();
    }
}

/// Registers a timer that fires `waker` when `deadline` is reached.
/// Returns `None` if `deadline` has already passed.
pub fn register_timer(deadline: TimeValue, waker: Waker) -> Option<TimerHandle> {
    let cpu_id = khal::percpu::this_cpu_id();
    let key = timer_runtime(cpu_id)
        .lock()
        .add_with_waker(deadline, waker)?;
    Some(TimerHandle { key, cpu_id })
}

/// Cancels a previously registered timer. Safe to call from any CPU.
pub fn cancel_timer(handle: &TimerHandle) {
    timer_runtime(handle.cpu_id).lock().cancel(&handle.key);
}

/// Future returned by `sleep` and `sleep_until`.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct TimerFuture {
    key: TimerKey,
    cpu_id: LogicalCpuId,
}

impl Future for TimerFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        timer_runtime(self.cpu_id).lock().poll(&self.key, cx)
    }
}

impl Drop for TimerFuture {
    fn drop(&mut self) {
        timer_runtime(self.cpu_id).lock().cancel(&self.key);
    }
}

/// Waits until `duration` has elapsed.
pub async fn sleep(duration: Duration) {
    sleep_until(monotonic_time() + duration).await
}

/// Waits until `deadline` is reached.
pub async fn sleep_until(deadline: TimeValue) {
    let cpu_id = khal::percpu::this_cpu_id();
    let key = timer_runtime(cpu_id).lock().add(deadline);
    if let Some(key) = key {
        TimerFuture { key, cpu_id }.await;
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
    duration: Option<Duration>,
    f: F,
) -> Result<F::Output, Elapsed> {
    timeout_at(
        duration.and_then(|x| x.checked_add(khal::time::monotonic_time())),
        f,
    )
    .await
}

/// Requires a `Future` to complete before the specified deadline.
pub async fn timeout_at<F: IntoFuture>(
    deadline: Option<TimeValue>,
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
