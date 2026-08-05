// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `timerfd` object implementation.

use alloc::{sync::Arc, task::Wake};
use core::{mem::size_of, task::Waker};

use kcred::Cred;
use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use kspin::SpinNoIrq;
use ktask::future::{TimerHandle, block_on, cancel_timer, poll_io, register_timer};
use ktime_types::{MonotonicInstant, SystemTime, TimeSpan};
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsInode};
use linux_raw_sys::general::{CLOCK_BOOTTIME, CLOCK_MONOTONIC};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimerDeadline {
    Monotonic(MonotonicInstant),
    Realtime(SystemTime),
}

impl TimerDeadline {
    fn from_absolute(clock_id: u32, value: TimeSpan) -> Option<Self> {
        match clock_id {
            CLOCK_MONOTONIC | CLOCK_BOOTTIME => Some(Self::Monotonic(
                MonotonicInstant::from_span_since_origin(value),
            )),
            _ => Some(Self::Realtime(SystemTime::from_unix_parts(
                i64::try_from(value.as_secs()).ok()?,
                value.subsec_nanos(),
            )?)),
        }
    }

    fn checked_add(self, span: TimeSpan) -> Option<Self> {
        match self {
            Self::Monotonic(instant) => instant.checked_add(span).map(Self::Monotonic),
            Self::Realtime(time) => time.checked_add(span).map(Self::Realtime),
        }
    }

    fn checked_duration_since(self, earlier: Self) -> Option<TimeSpan> {
        match (self, earlier) {
            (Self::Monotonic(now), Self::Monotonic(then)) => now.checked_duration_since(then),
            (Self::Realtime(now), Self::Realtime(then)) => now.duration_since(then).ok(),
            _ => None,
        }
    }

    fn is_before(self, other: Self) -> bool {
        match (self, other) {
            (Self::Monotonic(lhs), Self::Monotonic(rhs)) => lhs < rhs,
            (Self::Realtime(lhs), Self::Realtime(rhs)) => lhs < rhs,
            _ => false,
        }
    }

    fn monotonic_deadline(self) -> MonotonicInstant {
        match self {
            Self::Monotonic(deadline) => deadline,
            Self::Realtime(deadline) => ktime::realtime_deadline_to_monotonic(deadline),
        }
    }
}

fn clock_now(clock_id: u32) -> TimerDeadline {
    match clock_id {
        CLOCK_MONOTONIC | CLOCK_BOOTTIME => TimerDeadline::Monotonic(monotonic_time()),
        _ => TimerDeadline::Realtime(ktime::realtime()),
    }
}

struct TimerFdInner {
    expirations: u64,
    interval: TimeSpan,
    deadline: Option<TimerDeadline>,
}

impl TimerFdInner {
    fn tick(&mut self, now: TimerDeadline) -> bool {
        let deadline = match self.deadline {
            Some(deadline) => deadline,
            None => return self.expirations > 0,
        };
        if now.is_before(deadline) {
            return self.expirations > 0;
        }

        if self.interval.is_zero() {
            self.expirations = self.expirations.saturating_add(1);
            self.deadline = None;
        } else {
            let overdue = now
                .checked_duration_since(deadline)
                .expect("timerfd clock domain is fixed at creation");
            let periods = overdue.as_nanos() / self.interval.as_nanos() + 1;
            self.expirations = self
                .expirations
                .saturating_add(u64::try_from(periods).unwrap_or(u64::MAX));
            let advance = self
                .interval
                .as_nanos()
                .checked_mul(periods)
                .and_then(TimeSpan::try_from_nanos)
                .unwrap_or(TimeSpan::MAX);
            self.deadline = deadline.checked_add(advance);
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

/// File-like timer object for `timerfd_*` syscalls.
pub struct TimerFd {
    clock_id: u32,
    inner: SpinNoIrq<TimerFdInner>,
    poll_rx: PollSet,
    timer_handle: SpinNoIrq<Option<TimerHandle>>,
}

impl TimerFd {
    /// Create a new timerfd for the given clock.
    pub fn new(clock_id: u32) -> Arc<Self> {
        Arc::new(Self {
            clock_id,
            inner: SpinNoIrq::new(TimerFdInner {
                deadline: None,
                interval: TimeSpan::ZERO,
                expirations: 0,
            }),
            poll_rx: PollSet::new(),
            timer_handle: SpinNoIrq::new(None),
        })
    }

    /// Creates the timerfd anonymous-inode file and captures `cred` as its open credential.
    pub fn new_file(clock_id: u32, open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[timerfd]",
            Arc::new(TimerfdFops),
            Self::new(clock_id),
            FMode::READ | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the timerfd object attached to a timerfd file.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
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
            let deadline = match self.inner.lock().deadline {
                Some(deadline) => deadline,
                None => return,
            };

            let handle = register_timer(deadline.monotonic_deadline(), self.make_waker());
            if let Some(handle) = handle {
                *self.timer_handle.lock() = Some(handle);
                return;
            }

            let mut inner = self.inner.lock();
            let was_zero = inner.expirations == 0;
            let has_expiration = inner.tick(clock_now(self.clock_id));
            drop(inner);

            if has_expiration && was_zero {
                self.poll_rx.wake();
            }
        }
    }

    fn on_timer_expired(self: &Arc<Self>) {
        let mut inner = self.inner.lock();
        let was_zero = inner.expirations == 0;
        let has_expiration = inner.tick(clock_now(self.clock_id));
        drop(inner);

        if has_expiration && was_zero {
            self.poll_rx.wake();
        }
        self.arm_or_fire();
    }

    /// Program the timer and return the previous `(interval, remaining)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the requested deadline cannot be
    /// represented. Validation failures leave the existing timer unchanged.
    pub fn settime(
        self: &Arc<Self>,
        absolute: bool,
        value: TimeSpan,
        interval: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let now = clock_now(self.clock_id);
        let deadline = if value.is_zero() {
            None
        } else {
            Some(
                if absolute {
                    TimerDeadline::from_absolute(self.clock_id, value)
                } else {
                    now.checked_add(value)
                }
                .ok_or(KError::InvalidInput)?,
            )
        };

        self.cancel_pending_timer();

        let mut inner = self.inner.lock();
        let old_interval = inner.interval;
        let old_remaining = inner
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(now))
            .unwrap_or(TimeSpan::ZERO);

        let Some(deadline) = deadline else {
            inner.deadline = None;
            inner.interval = TimeSpan::ZERO;
            inner.expirations = 0;
            return Ok((old_interval, old_remaining));
        };

        inner.deadline = Some(deadline);
        inner.interval = interval;
        inner.expirations = 0;

        let fired = inner.tick(now);
        drop(inner);

        if fired {
            self.poll_rx.wake();
        }
        self.arm_or_fire();

        Ok((old_interval, old_remaining))
    }

    /// Query the current timer setting as `(interval, remaining)`.
    pub fn gettime(&self) -> (TimeSpan, TimeSpan) {
        let inner = self.inner.lock();
        let remaining = inner
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(clock_now(self.clock_id)))
            .unwrap_or(TimeSpan::ZERO);
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

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let mut inner = self.inner.lock();
        inner.tick(clock_now(self.clock_id));
        events.set(IoEvents::IN, inner.expirations > 0);
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

struct TimerfdFops;

impl TimerfdFops {
    fn timerfd(file: &VfsFile) -> KResult<Arc<TimerFd>> {
        TimerFd::from_file(file)
    }
}

impl FileOperations for TimerfdFops {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        if buf.len() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        let timerfd = Self::timerfd(file)?;
        block_on(poll_io(
            timerfd.as_ref(),
            IoEvents::IN,
            file.is_nonblocking(),
            || {
                let mut inner = timerfd.inner.lock();
                inner.tick(clock_now(timerfd.clock_id));
                if inner.expirations > 0 {
                    let count = inner.expirations;
                    inner.expirations = 0;
                    drop(inner);
                    buf[..size_of::<u64>()].copy_from_slice(&count.to_ne_bytes());
                    Ok(size_of::<u64>())
                } else {
                    Err(KError::WouldBlock)
                }
            },
        ))
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::timerfd(file).map_or(IoEvents::ERR, |timerfd| timerfd.poll())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(timerfd) = Self::timerfd(file) {
            timerfd.register(context, events)?;
        }
        Ok(())
    }
}

#[cfg(unittest)]
mod timerfd_tests {
    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;

    fn monotonic(span: TimeSpan) -> TimerDeadline {
        TimerDeadline::Monotonic(MonotonicInstant::from_span_since_origin(span))
    }

    #[def_test]
    fn test_timerfd_creation() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        assert!(!tfd.poll().contains(IoEvents::IN));
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
        let file = TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred())
            .expect("timerfd file opens");
        assert!(!file.is_nonblocking());
        file.set_nonblocking(true);
        assert!(file.is_nonblocking());
        file.set_nonblocking(false);
        assert!(!file.is_nonblocking());
    }

    #[def_test]
    fn test_timerfd_settime_disarm() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let (old_interval, old_remaining) = tfd
            .settime(false, TimeSpan::from_secs(10), TimeSpan::ZERO)
            .unwrap();
        assert!(old_interval.is_zero());
        assert!(old_remaining.is_zero());

        let (old_interval, _) = tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
        assert!(old_interval.is_zero());

        let (interval, remaining) = tfd.gettime();
        assert!(interval.is_zero());
        assert!(remaining.is_zero());
    }

    #[def_test]
    fn test_timerfd_invalid_deadline_preserves_state() {
        let tfd = TimerFd::new(linux_raw_sys::general::CLOCK_REALTIME);
        let deadline = TimerDeadline::Realtime(SystemTime::from_unix_seconds(100));
        {
            let mut inner = tfd.inner.lock();
            inner.deadline = Some(deadline);
            inner.interval = TimeSpan::from_secs(2);
            inner.expirations = 3;
        }

        assert_eq!(
            tfd.settime(true, TimeSpan::MAX, TimeSpan::ZERO),
            Err(KError::InvalidInput)
        );

        let inner = tfd.inner.lock();
        assert_eq!(inner.deadline, Some(deadline));
        assert_eq!(inner.interval, TimeSpan::from_secs(2));
        assert_eq!(inner.expirations, 3);
    }

    #[def_test]
    fn test_timerfd_already_expired() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        tfd.settime(false, TimeSpan::from_nanos(1), TimeSpan::ZERO)
            .unwrap();
        assert!(tfd.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_timerfd_read_small_buffer() {
        let file = TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred())
            .expect("timerfd file opens");
        let mut small_out = [0u8; 4];
        let mut pos = 0;
        assert_eq!(
            file.read_from(&mut small_out, &mut pos),
            Err(KError::InvalidInput)
        );
    }

    #[def_test]
    fn test_timerfd_write_returns_error() {
        let file = TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred())
            .expect("timerfd file opens");
        let data = b"test1234";
        let mut pos = 0;
        assert!(file.write_from(data, &mut pos).is_err());
    }

    #[def_test]
    fn test_timerfd_tick_oneshot() {
        let mut inner = TimerFdInner {
            expirations: 0,
            interval: TimeSpan::ZERO,
            deadline: Some(monotonic(TimeSpan::from_secs(100))),
        };
        assert!(!inner.tick(monotonic(TimeSpan::from_secs(99))));
        assert_eq!(inner.expirations, 0);
        assert!(inner.deadline.is_some());

        assert!(inner.tick(monotonic(TimeSpan::from_secs(100))));
        assert_eq!(inner.expirations, 1);
        assert!(inner.deadline.is_none());
    }

    #[def_test]
    fn test_timerfd_tick_periodic() {
        let mut inner = TimerFdInner {
            expirations: 0,
            interval: TimeSpan::from_millis(100),
            deadline: Some(monotonic(TimeSpan::from_secs(10))),
        };
        assert!(inner.tick(monotonic(TimeSpan::from_millis(10_350))));
        assert_eq!(inner.expirations, 4);
        assert_eq!(
            inner.deadline,
            Some(monotonic(TimeSpan::from_millis(10_400)))
        );
    }

    #[def_test]
    fn test_timerfd_tick_periodic_accumulates() {
        let mut inner = TimerFdInner {
            expirations: 2,
            interval: TimeSpan::from_millis(100),
            deadline: Some(monotonic(TimeSpan::from_secs(10))),
        };
        assert!(inner.tick(monotonic(TimeSpan::from_millis(10_050))));
        assert_eq!(inner.expirations, 3);
    }

    #[def_test]
    fn test_timerfd_settime_rearm_returns_old() {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        tfd.settime(false, TimeSpan::from_secs(10), TimeSpan::from_secs(1))
            .unwrap();

        let (old_interval, old_remaining) = tfd
            .settime(false, TimeSpan::from_secs(5), TimeSpan::ZERO)
            .unwrap();
        assert_eq!(old_interval, TimeSpan::from_secs(1));
        assert!(old_remaining > TimeSpan::ZERO);
    }
}
