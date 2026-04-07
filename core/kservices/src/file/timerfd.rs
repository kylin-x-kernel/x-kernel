// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `timerfd` implementation.
//!
//! Armed timers register into the percpu `TimerRuntime` via
//! [`ktask::future::register_timer`]; on expiry the waker accumulates
//! expirations and re-registers periodic timers

use alloc::{borrow::Cow, sync::Arc, task::Wake};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Waker},
    time::Duration,
};

use kerrno::{KError, KResult};
use khal::time::{self, monotonic_time, wall_time};
use kpoll::{IoEvents, PollSet, Pollable};
use kspin::SpinNoIrq;
use ktask::future::{TimerHandle, block_on, cancel_timer, poll_io, register_timer};
use linux_raw_sys::general::{CLOCK_BOOTTIME, CLOCK_MONOTONIC};

use crate::file::{FileLike, IoDst, IoSrc};

fn clock_now(clock_id: u32) -> Duration {
    match clock_id {
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => monotonic_time(),
        _ => wall_time(),
    }
}

/// Convert a clock-domain deadline to wall-time domain (TimerRuntime uses `wall_time()`).
fn to_wall_deadline(clock_id: u32, deadline: Duration) -> Duration {
    match clock_id {
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => deadline + Duration::from_nanos(time::offset_ns()),
        _ => deadline,
    }
}

struct TimerFdInner {
    expirations: u64,
    interval: Duration,
    deadline: Option<Duration>,
}

impl TimerFdInner {
    /// Advance the timer state to `now`, accumulating expirations.
    /// Returns `true` if expirations > 0 after the update.
    fn tick(&mut self, now: Duration) -> bool {
        let deadline = match self.deadline {
            Some(d) => d,
            None => return self.expirations > 0,
        };

        if now < deadline {
            return self.expirations > 0;
        }

        if self.interval.is_zero() {
            self.expirations += 1;
            self.deadline = None;
        } else {
            let overdue = now - deadline;
            let periods = overdue.as_nanos() / self.interval.as_nanos() + 1;
            self.expirations += periods as u64;
            let advance_nanos = self.interval.as_nanos() * periods;
            self.deadline = Some(deadline + Duration::from_nanos(advance_nanos as u64));
        }

        true
    }
}

struct TimerFdWaker(Arc<TimerFd>);

impl Wake for TimerFdWaker {
    fn wake(self: Arc<Self>) {
        self.0.on_timer_expired();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.on_timer_expired();
    }
}

/// Kernel object implementing Linux timerfd semantics.
pub struct TimerFd {
    clock_id: u32,
    inner: SpinNoIrq<TimerFdInner>,
    non_blocking: AtomicBool,
    timer_handle: SpinNoIrq<Option<TimerHandle>>,
    poll_rx: PollSet,
}

impl TimerFd {
    /// Create a new disarmed timer fd.
    pub fn new(clock_id: u32) -> Arc<Self> {
        Arc::new(Self {
            clock_id,
            inner: SpinNoIrq::new(TimerFdInner {
                expirations: 0,
                interval: Duration::ZERO,
                deadline: None,
            }),
            non_blocking: AtomicBool::new(false),
            timer_handle: SpinNoIrq::new(None),
            poll_rx: PollSet::new(),
        })
    }

    fn cancel_pending_timer(&self) {
        if let Some(handle) = self.timer_handle.lock().take() {
            cancel_timer(&handle);
        }
    }

    fn make_waker(self: &Arc<Self>) -> Waker {
        Waker::from(Arc::new(TimerFdWaker(self.clone())))
    }

    fn arm_or_fire(self: &Arc<Self>) {
        loop {
            let dl = match self.inner.lock().deadline {
                Some(dl) => dl,
                None => return,
            };

            let handle = register_timer(to_wall_deadline(self.clock_id, dl), self.make_waker());
            if let Some(h) = handle {
                *self.timer_handle.lock() = Some(h);
                return;
            }

            let mut inner = self.inner.lock();
            let was_zero = inner.expirations == 0;
            let has_exp = inner.tick(clock_now(self.clock_id));
            drop(inner);

            if has_exp && was_zero {
                self.poll_rx.wake();
            }
        }
    }

    fn on_timer_expired(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        let was_zero = inner.expirations == 0;
        let has_exp = inner.tick(clock_now(self.clock_id));
        drop(inner);

        if has_exp && was_zero {
            self.poll_rx.wake();
        }

        self.arm_or_fire();
    }

    /// Arm or disarm the timer.
    ///
    /// Returns `(old_interval, old_remaining)`.
    pub fn settime(
        self: &Arc<Self>,
        absolute: bool,
        value: Duration,
        interval: Duration,
    ) -> (Duration, Duration) {
        self.cancel_pending_timer();

        let now = clock_now(self.clock_id);
        let mut inner = self.inner.lock();

        let old_interval = inner.interval;
        let old_remaining = inner
            .deadline
            .and_then(|d| d.checked_sub(now))
            .unwrap_or(Duration::ZERO);

        if value.is_zero() {
            inner.deadline = None;
            inner.interval = Duration::ZERO;
            inner.expirations = 0;
            return (old_interval, old_remaining);
        }

        let deadline = if absolute { value } else { now + value };

        inner.deadline = Some(deadline);
        inner.interval = interval;
        inner.expirations = 0;

        let fired = inner.tick(now);
        drop(inner);

        if fired {
            self.poll_rx.wake();
        }

        self.arm_or_fire();

        (old_interval, old_remaining)
    }

    /// Query the current timer setting.
    ///
    /// Returns `(interval, remaining)`.
    pub fn gettime(&self) -> (Duration, Duration) {
        let inner = self.inner.lock();
        let remaining = inner
            .deadline
            .and_then(|d| d.checked_sub(clock_now(self.clock_id)))
            .unwrap_or(Duration::ZERO);
        (inner.interval, remaining)
    }
}

impl Drop for TimerFd {
    fn drop(&mut self) {
        if let Some(handle) = self.timer_handle.get_mut().take() {
            cancel_timer(&handle);
        }
    }
}

impl FileLike for TimerFd {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let mut inner = self.inner.lock();
            inner.tick(clock_now(self.clock_id));
            if inner.expirations > 0 {
                let count = inner.expirations;
                inner.expirations = 0;
                drop(inner);
                dst.write(&count.to_ne_bytes())?;
                Ok(size_of::<u64>())
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> KResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[timerfd]".into()
    }
}

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let mut inner = self.inner.lock();
        inner.tick(clock_now(self.clock_id));
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, inner.expirations > 0);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
    }
}

#[cfg(unittest)]
mod timerfd_tests {
    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_timerfd_creation() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        assert_eq!(tfd.path(), "anon_inode:[timerfd]");
        assert!(!tfd.nonblocking());
    }

    #[def_test]
    fn test_timerfd_disarmed_poll() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        assert!(!tfd.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_timerfd_gettime_disarmed() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let (interval, remaining) = tfd.gettime();
        assert!(interval.is_zero());
        assert!(remaining.is_zero());
    }

    #[def_test]
    fn test_timerfd_nonblocking() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        tfd.set_nonblocking(true).unwrap();
        assert!(tfd.nonblocking());
        tfd.set_nonblocking(false).unwrap();
        assert!(!tfd.nonblocking());
    }

    #[def_test]
    fn test_timerfd_settime_disarm() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let (old_interval, old_remaining) =
            tfd.settime(false, Duration::from_secs(10), Duration::ZERO);
        assert!(old_interval.is_zero());
        assert!(old_remaining.is_zero());

        // Disarm.
        let (old_interval, _) = tfd.settime(false, Duration::ZERO, Duration::ZERO);
        assert!(old_interval.is_zero());

        let (interval, remaining) = tfd.gettime();
        assert!(interval.is_zero());
        assert!(remaining.is_zero());
    }

    #[def_test]
    fn test_timerfd_already_expired() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        tfd.settime(false, Duration::from_nanos(1), Duration::ZERO);
        assert!(tfd.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_timerfd_read_small_buffer() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let mut small_out = [0u8; 4];
        let mut dst = kio::Cursor::new(small_out.as_mut_slice());
        assert_eq!(tfd.read(&mut dst), Err(KError::InvalidInput));
    }

    #[def_test]
    fn test_timerfd_write_returns_error() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let data = b"test1234";
        let mut src = kio::Cursor::new(data.as_slice());
        assert!(tfd.write(&mut src).is_err());
    }

    #[def_test]
    fn test_timerfd_tick_oneshot() {
        let mut inner = TimerFdInner {
            expirations: 0,
            interval: Duration::ZERO,
            deadline: Some(Duration::from_secs(100)),
        };
        assert!(!inner.tick(Duration::from_secs(99)));
        assert_eq!(inner.expirations, 0);
        assert!(inner.deadline.is_some());

        assert!(inner.tick(Duration::from_secs(100)));
        assert_eq!(inner.expirations, 1);
        assert!(inner.deadline.is_none());
    }

    #[def_test]
    fn test_timerfd_tick_periodic() {
        let mut inner = TimerFdInner {
            expirations: 0,
            interval: Duration::from_millis(100),
            deadline: Some(Duration::from_secs(10)),
        };
        // 350ms overdue -> 4 expirations (at +0, +100, +200, +300ms).
        assert!(inner.tick(Duration::from_millis(10_350)));
        assert_eq!(inner.expirations, 4);
        assert_eq!(inner.deadline, Some(Duration::from_millis(10_400)));
    }

    #[def_test]
    fn test_timerfd_tick_periodic_accumulates() {
        let mut inner = TimerFdInner {
            expirations: 2,
            interval: Duration::from_millis(100),
            deadline: Some(Duration::from_secs(10)),
        };
        assert!(inner.tick(Duration::from_millis(10_050)));
        assert_eq!(inner.expirations, 3);
    }

    #[def_test]
    fn test_timerfd_settime_rearm_returns_old() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        // Arm with 10s interval 1s.
        tfd.settime(false, Duration::from_secs(10), Duration::from_secs(1));

        let (old_interval, old_remaining) =
            tfd.settime(false, Duration::from_secs(5), Duration::ZERO);
        assert_eq!(old_interval, Duration::from_secs(1));
        assert!(old_remaining > Duration::ZERO);
    }
}
