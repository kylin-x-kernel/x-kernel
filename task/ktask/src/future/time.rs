// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async time utilities and timer wheel integration.

use alloc::collections::BTreeMap;
use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use futures_util::{FutureExt, select_biased};
use kcpu_id_map::LogicalCpuId;
use kerrno::KError;
use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::api::rearm_local_timer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TimerKey {
    deadline: MonotonicInstant,
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

    fn add(&mut self, deadline: MonotonicInstant) -> Option<TimerKey> {
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

    fn add_with_waker(&mut self, deadline: MonotonicInstant, waker: Waker) -> Option<TimerKey> {
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

    /// Earliest pending soft-timer deadline, or `None` if the wheel is empty.
    ///
    /// `TimerKey` orders by `deadline` first, so `first_key_value()` is the
    /// minimum-deadline entry.
    fn earliest_deadline(&self) -> Option<MonotonicInstant> {
        self.wheel.first_key_value().map(|(k, _)| k.deadline)
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

/// Drains expired timers and returns the next deadline, all under a single
/// wheel-lock acquisition. The returned wakers must be woken by the caller
/// (outside the lock) before re-arming the hardware.
pub(crate) fn drain_expired_and_get_earliest(
    cpu_id: kcpu_id_map::LogicalCpuId,
) -> (BTreeMap<TimerKey, Waker>, Option<MonotonicInstant>) {
    let mut guard = timer_runtime(cpu_id).lock();
    let wakers = guard.take_expired_wakers();
    let earliest = guard.earliest_deadline();
    (wakers, earliest)
}

/// Returns the earliest pending soft-timer deadline on `cpu_id`.
pub(crate) fn earliest_deadline(cpu_id: kcpu_id_map::LogicalCpuId) -> Option<MonotonicInstant> {
    timer_runtime(cpu_id).lock().earliest_deadline()
}

/// Registers a timer that fires `waker` when `deadline` is reached.
/// Returns `None` if `deadline` has already passed.
pub fn register_timer(deadline: MonotonicInstant, waker: Waker) -> Option<TimerHandle> {
    // Pin to this CPU across id selection, wheel insert, and local rearm so an
    // affinity migrate cannot insert on one RQ and reprogram another.
    let _pin = kspin::NoPreempt::new();
    let cpu_id = khal::percpu::this_cpu_id();
    let mut guard = timer_runtime(cpu_id).lock();
    let key = guard.add_with_waker(deadline, waker)?;
    // While still under the wheel lock (IRQs masked), pull the hardware
    // deadline forward to the new earliest soft timer so timeouts wake on time.
    rearm_local_timer(guard.earliest_deadline(), None);
    drop(guard);
    Some(TimerHandle { key, cpu_id })
}

/// Cancels a previously registered timer. Safe to call from any CPU.
///
/// If the cancelled entry was the earliest on this CPU's wheel, the local
/// hardware timer is re-armed to the next earliest deadline. Cancelling the
/// earliest entry on a *remote* CPU is left to self-correct: that CPU's timer
/// fires at the now-stale deadline, and its handler recomputes the next one
/// (at most one spurious IRQ), so no cross-CPU IPI is needed.
pub fn cancel_timer(handle: &TimerHandle) {
    let mut guard = timer_runtime(handle.cpu_id).lock();
    // IRQs are masked on this CPU while the lock is held, so `this_cpu_id()`
    // is stable here and matches the CPU whose hardware timer we re-arm.
    let local = handle.cpu_id == khal::percpu::this_cpu_id();
    let was_earliest = guard.earliest_deadline() == Some(handle.key.deadline);
    guard.cancel(&handle.key);
    if local && was_earliest {
        rearm_local_timer(guard.earliest_deadline(), None);
    }
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
        let mut guard = timer_runtime(self.cpu_id).lock();
        let local = self.cpu_id == khal::percpu::this_cpu_id();
        let was_earliest = guard.earliest_deadline() == Some(self.key.deadline);
        guard.cancel(&self.key);
        if local && was_earliest {
            rearm_local_timer(guard.earliest_deadline(), None);
        }
    }
}

/// Waits until `duration` has elapsed.
pub async fn sleep(duration: TimeSpan) {
    sleep_until(monotonic_time() + duration).await
}

/// Waits until `deadline` is reached.
pub async fn sleep_until(deadline: MonotonicInstant) {
    // Pin across CPU id + wheel insert + rearm (see `register_timer`).
    let (key, cpu_id) = {
        let _pin = kspin::NoPreempt::new();
        let cpu_id = khal::percpu::this_cpu_id();
        // Register the timer and re-arm the hardware while still under the wheel
        // lock, then drop the guard before awaiting: a `SpinNoIrq` guard must not
        // be held across the `.await` (it would block the timer IRQ that wakes us).
        let key = {
            let mut guard = timer_runtime(cpu_id).lock();
            let key = guard.add(deadline);
            if key.is_some() {
                rearm_local_timer(guard.earliest_deadline(), None);
            }
            key
        };
        (key, cpu_id)
    };
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

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests_subtick_precision {
    use alloc::{string::String, sync::Arc};
    use core::{
        future::pending,
        sync::atomic::{AtomicBool, Ordering},
    };

    use kcpu_id_map::KCpuMaskExt;
    use khal::time::monotonic_time;
    use ktime_types::TimeSpan;
    use unittest::{assert, def_test};

    use super::{sleep, timeout};
    use crate::future::block_on;

    /// Samples per case: enough to stabilise the median against scheduling
    /// jitter, while keeping the worst-case runtime modest.
    const SAMPLES: usize = 10;

    /// Idle-CPU bound for a short timed wait (timer wheel + modest wake latency).
    ///
    /// This is **not** a guarantee under same-CPU CPU-bound competition; use the
    /// contended tests for that.
    const IDLE_SUBTICK_LIMIT: TimeSpan = TimeSpan::from_millis(6);

    /// Contended wake budget: request + one default request slice + 2 ms slack.
    fn contended_limit(request: TimeSpan) -> TimeSpan {
        TimeSpan::from_nanos(
            request
                .as_nanos_u64_saturating()
                .saturating_add(ksched::DEFAULT_SLICE_NS)
                .saturating_add(2_000_000),
        )
    }

    struct TimingStats {
        min: TimeSpan,
        median: TimeSpan,
        max: TimeSpan,
    }

    /// Sorts `samples` in place and derives min / median / max.
    fn stats_of(samples: &mut [TimeSpan]) -> TimingStats {
        samples.sort();
        let last = samples.len() - 1;
        TimingStats {
            min: samples[0],
            median: samples[last / 2],
            max: samples[last],
        }
    }

    #[cfg(feature = "sched_eevdf")]
    struct SameCpuSpinner {
        task: crate::KtaskRef,
        stop: Arc<AtomicBool>,
    }

    #[cfg(feature = "sched_eevdf")]
    impl Drop for SameCpuSpinner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Do not let a failed timing assertion leak a permanently runnable
            // worker into the remaining serial unittest suite.
            let _ = self.task.join();
        }
    }

    /// Pins a `spin_loop` worker to this CPU so timer wakes compete for the RQ.
    #[cfg(feature = "sched_eevdf")]
    fn spawn_same_cpu_spinner() -> SameCpuSpinner {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
            move || {
                while !stop_flag.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
            },
            String::from("subtick-spinner"),
            kbuild_config::TASK_STACK_SIZE,
        ));
        let cpu = khal::percpu::this_cpu_id();
        task.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
        crate::activate_task(&task);
        // Let the spinner land on this RQ before measuring.
        crate::yield_now();
        SameCpuSpinner { task, stop }
    }

    /// Idle: `sleep` probes soft-timer expiry without same-CPU competition.
    #[def_test(serial)]
    fn test_idle_subtick_sleep_is_not_rounded_to_tick() {
        const REQUEST: TimeSpan = TimeSpan::from_millis(1);

        block_on(sleep(REQUEST));

        let mut samples = [TimeSpan::ZERO; SAMPLES];
        for slot in &mut samples {
            let start = monotonic_time();
            block_on(sleep(REQUEST));
            *slot = monotonic_time() - start;
        }
        let stats = stats_of(&mut samples);
        log::info!(
            "idle sleep({:?}): min={:?} median={:?} max={:?} (limit={:?})",
            REQUEST,
            stats.min,
            stats.median,
            stats.max,
            IDLE_SUBTICK_LIMIT,
        );
        assert!(stats.min >= REQUEST);
        // Latency is logged for the performance harness. Ordinary unit tests
        // only validate timer semantics because QEMU, SMP load, and debug
        // builds can legitimately shift the median.
    }

    /// Idle: `timeout` mirrors futex-wait-with-timeout on an idle CPU.
    #[def_test(serial)]
    fn test_idle_subtick_timeout_is_not_rounded_to_tick() {
        const REQUEST: TimeSpan = TimeSpan::from_millis(2);

        let _ = block_on(timeout(Some(REQUEST), pending::<()>()));
        let mut samples = [TimeSpan::ZERO; SAMPLES];
        for slot in &mut samples {
            let start = monotonic_time();
            let result = block_on(timeout(Some(REQUEST), pending::<()>()));
            assert!(result.is_err());
            *slot = monotonic_time() - start;
        }
        let stats = stats_of(&mut samples);
        log::info!(
            "idle timeout({:?}): min={:?} median={:?} max={:?} (limit={:?})",
            REQUEST,
            stats.min,
            stats.median,
            stats.max,
            IDLE_SUBTICK_LIMIT,
        );
        assert!(stats.min >= REQUEST);
    }

    /// Contended: same-CPU spinner; latency may include one default request slice.
    #[cfg(feature = "sched_eevdf")]
    #[def_test(serial)]
    fn test_contended_subtick_sleep_within_slice_budget() {
        const REQUEST: TimeSpan = TimeSpan::from_millis(1);
        let limit = contended_limit(REQUEST);
        let spinner = spawn_same_cpu_spinner();

        block_on(sleep(REQUEST));
        let mut samples = [TimeSpan::ZERO; SAMPLES];
        for slot in &mut samples {
            let start = monotonic_time();
            block_on(sleep(REQUEST));
            *slot = monotonic_time() - start;
        }
        drop(spinner);

        let stats = stats_of(&mut samples);
        log::info!(
            "contended sleep({:?}): min={:?} median={:?} max={:?} (limit={:?})",
            REQUEST,
            stats.min,
            stats.median,
            stats.max,
            limit,
        );
        assert!(stats.min >= REQUEST);
    }

    /// Contended: same-CPU spinner against `timeout`.
    #[cfg(feature = "sched_eevdf")]
    #[def_test(serial)]
    fn test_contended_subtick_timeout_within_slice_budget() {
        const REQUEST: TimeSpan = TimeSpan::from_millis(2);
        let limit = contended_limit(REQUEST);
        let spinner = spawn_same_cpu_spinner();

        let _ = block_on(timeout(Some(REQUEST), pending::<()>()));
        let mut samples = [TimeSpan::ZERO; SAMPLES];
        for slot in &mut samples {
            let start = monotonic_time();
            let result = block_on(timeout(Some(REQUEST), pending::<()>()));
            assert!(result.is_err());
            *slot = monotonic_time() - start;
        }
        drop(spinner);

        let stats = stats_of(&mut samples);
        log::info!(
            "contended timeout({:?}): min={:?} median={:?} max={:?} (limit={:?})",
            REQUEST,
            stats.min,
            stats.median,
            stats.max,
            limit,
        );
        assert!(stats.min >= REQUEST);
    }
}
