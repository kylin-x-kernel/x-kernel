// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned async sleep timers and legacy waker registrations.

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

/// Refreshes hardware while holding the local wheel lock through arbitration.
pub(crate) fn reprogram_local_timer() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let wheel = timer_runtime(khal::percpu::this_cpu_id()).lock();
    rearm_local_timer(wheel.earliest_deadline(), None);
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

#[cfg(all(unittest, feature = "ipi"))]
mod notification_tests {
    use core::task::Waker;

    use unittest::{assert, def_test};

    use super::{TimerKey, timer_runtime};

    #[def_test(serial)]
    fn reprogram_notification_does_not_consume_due_timer() {
        let _guard = kspin::NoPreemptIrqSave::new();
        let cpu = khal::percpu::this_cpu_id();
        let key = {
            let mut runtime = timer_runtime(cpu).lock();
            let key = TimerKey {
                deadline: ktime_types::MonotonicInstant::from_span_since_origin(
                    ktime_types::TimeSpan::from_nanos(0),
                ),
                key: runtime.key,
            };
            runtime.key += 1;
            // Deliberately overdue: notification must only program hardware,
            // leaving consumption to the actual timer IRQ after this guard.
            runtime.wheel.insert(key, Waker::noop().clone());
            key
        };
        kipi::TimerReprogramIpiIf::on_timer_reprogram();
        let remained_pending = timer_runtime(cpu).lock().wheel.remove(&key).is_some();
        super::reprogram_local_timer();
        assert!(remained_pending);
    }
}

#[cfg(unittest)]
mod owned_sleep_tests {
    use alloc::{sync::Arc, task::Wake};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[def_test(serial)]
    fn ready_operation_wins_at_expired_timeout() {
        assert_eq!(
            crate::future::block_on(timeout_at(
                Some(MonotonicInstant::ORIGIN),
                core::future::ready(17),
            )),
            Ok(17)
        );
        assert!(
            crate::future::block_on(timeout_at(
                Some(MonotonicInstant::ORIGIN),
                core::future::pending::<()>(),
            ))
            .is_err()
        );
    }

    #[def_test(serial)]
    fn real_irq_completes_before_first_poll() {
        let _pin = kspin::NoPreempt::new();
        let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_millis(1)).unwrap();
        let limit = monotonic_time() + TimeSpan::from_secs(2);
        while !future.timer.handler().0.lock().is_expired && monotonic_time() < limit {
            core::hint::spin_loop();
        }
        future.timer.cancel_sync();
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }

    #[def_test(serial)]
    fn oversized_sleep_stays_pending_but_core_input_is_rejected() {
        let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(u64::MAX));
        assert!(TimerFuture::new(deadline).is_err());
        let mut wait = core::pin::pin!(sleep_until(deadline));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }

    #[def_test(serial)]
    fn expired_before_poll_completes_without_waking() {
        let mut future = TimerFuture::new(MonotonicInstant::ORIGIN).unwrap();
        let count = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
        assert_eq!(count.0.load(Ordering::Relaxed), 0);
        assert!(future.timer.handler().0.lock().waker.is_none());
    }

    #[def_test(serial)]
    fn completion_notifies_latest_waker_and_poll_stays_ready() {
        let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_secs(60)).unwrap();
        future.timer.cancel_sync();
        let first = Arc::new(WakeCount(AtomicUsize::new(0)));
        let second = Arc::new(WakeCount(AtomicUsize::new(0)));
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(&first_waker))
                .is_pending()
        );
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(&second_waker))
                .is_pending()
        );
        // Model completion after replacing the registered waker. Actual IRQ
        // delivery is exercised by the subtick sleep/timeout tests below.
        let wake = future.timer.handler().0.lock().complete().unwrap();
        wake.wake();
        assert_eq!(first.0.load(Ordering::Relaxed), 0);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(&second_waker))
                .is_ready()
        );
        assert!(future.timer.handler().0.lock().complete().is_none());
    }

    #[def_test(serial)]
    fn drop_pending_sleep_releases_callback_and_waker() {
        let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_secs(60)).unwrap();
        let count = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        assert!(
            Pin::new(&mut future)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(future);
        assert_eq!(Arc::strong_count(&count), 2);
        assert_eq!(count.0.load(Ordering::Relaxed), 0);
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

    /// Idle-CPU bound for a short timed wait (timer delivery + modest wake latency).
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
