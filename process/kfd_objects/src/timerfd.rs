// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File-backed timer expiration accounting on an owned, reusable one-shot timer.
//!
//! [`TimerFd::new_file`] creates the file used by syscall adapters. Periodic
//! counts are maintained here, not in the timer core; reading an expiration
//! count rearms its next notification. See the crate design for locking and
//! the final-owner teardown contract.
//! File reads recheck expirations after signal interruption; a still-empty wait
//! reports the VFS file-wait marker. Syscall return owns handler restart policy.

use alloc::sync::Arc;
use core::mem::size_of;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use kspin::SpinNoIrq;
use ksync::Mutex;
use ktask::future::block_on;
use ktime_types::{MonotonicInstant, SystemTime, TimeSpan};
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsInode, poll_file_io};
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
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The deadline and current time use \
                          the immutable timerfd clock domain and now was checked not earlier"
            )]
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

// State lives beside the prepared node and never owns the FD. Final FD release
// drains delivery before destroying this handler, without a second shared owner.
struct TimerFdHandler {
    clock_id: u32,
    inner: SpinNoIrq<TimerFdInner>,
    poll_rx: PollSet,
}

fn core_deadline(deadline: TimerDeadline) -> MonotonicInstant {
    let horizon = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
    deadline.monotonic_deadline().min(horizon)
}

impl ktask::TimerHandler for TimerFdHandler {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        let mut inner = self.inner.lock();
        let has_expiration = inner.tick(clock_now(self.clock_id));
        if !has_expiration && let Some(deadline) = inner.deadline {
            // Realtime may have moved backwards. Normal expiration stays
            // disarmed until read; tiny user intervals must not loop in IRQ.
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: core_deadline bounds the deadline \
                          and the initialized task or timer callback supplies a valid CPU route"
            )]
            timer
                .rearm(core_deadline(deadline))
                .expect("bounded timerfd deadline is valid");
        }
        drop(inner);
        // A competing poll may have accounted expiration without notifying
        // existing waiters. Delivery must still publish that readiness.
        if has_expiration {
            self.poll_rx.wake();
        }
    }
}

/// File-like timer object for timerfd syscalls.
pub struct TimerFd {
    // Timer Drop drains the callback before destroying its embedded handler state.
    timer: ktask::Timer<TimerFdHandler>,
    // Settime/read serialize programming; callbacks/poll/gettime never take it.
    programming: Mutex<()>,
}

impl TimerFd {
    /// Creates a disarmed timer for a caller-validated clock ID.
    ///
    /// MONOTONIC and BOOTTIME use monotonic time; other IDs select realtime.
    /// The caller must initialize the timer runtime before programming it.
    /// Preparation allocates; subsequent programming reuses the timer storage.
    ///
    /// # Panics
    /// Panics if the timer provider violates its newly allocated exclusive
    /// storage invariant. Allocation failure follows the kernel allocator policy.
    pub fn new(clock_id: u32) -> Arc<Self> {
        let handler = TimerFdHandler {
            clock_id,
            inner: SpinNoIrq::new(TimerFdInner {
                deadline: None,
                interval: TimeSpan::ZERO,
                expirations: 0,
            }),
            poll_rx: PollSet::new(),
        };
        Arc::new(Self {
            timer: ktask::Timer::new(handler),
            programming: Mutex::new(()),
        })
    }

    /// Creates the timerfd anonymous-inode file and captures `cred` as its open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from anonymous-inode file creation.
    ///
    /// # Panics
    /// Has the timer-preparation invariant panic described by [`Self::new`].
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
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Programs the timer and returns the previous (interval, remaining) pair.
    /// Runs in sleepable task context; concurrent programming is serialized.
    /// Zero value disarms and clears unread expirations. A periodic timer keeps
    /// accumulating elapsed intervals while unread, without recurring IRQ work;
    /// the next notification is armed when a read consumes its count.
    ///
    /// # Errors
    ///
    /// Returns [`KError::InvalidInput`] if the requested deadline cannot be
    /// represented. Validation failures leave the existing timer unchanged.
    ///
    /// # Panics
    /// Panics if initialized CPU routing or hardware notification fails, or if
    /// internal clock-domain/bounded-deadline invariants are violated.
    pub fn settime(
        &self,
        absolute: bool,
        value: TimeSpan,
        interval: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let state = self.timer.handler();
        let now = clock_now(state.clock_id);
        let deadline = if value.is_zero() {
            None
        } else {
            Some(
                if absolute {
                    TimerDeadline::from_absolute(state.clock_id, value)
                } else {
                    now.checked_add(value)
                }
                .ok_or(KError::InvalidInput)?,
            )
        };

        let programming = self.programming.lock();
        // Never wait with the callback's state lock held. Programming requests
        // serialize here; the callback only takes handler state and its core lock.
        self.timer.cancel_sync();
        let mut inner = state.inner.lock();
        let now = clock_now(state.clock_id);
        inner.tick(now);
        let old_interval = inner.interval;
        let old_remaining = inner
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(now))
            .unwrap_or(TimeSpan::ZERO);
        inner.deadline = deadline;
        inner.interval = if deadline.is_some() {
            interval
        } else {
            TimeSpan::ZERO
        };
        inner.expirations = 0;
        let fired = inner.tick(clock_now(state.clock_id));
        if !fired && let Some(deadline) = inner.deadline {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: core_deadline bounds the deadline \
                          and the initialized task or timer callback supplies a valid CPU route"
            )]
            // SAFETY: settime runs in initialized task context. The FD retains
            // stable handler state; the handler preserves hard-IRQ context
            // and only updates locked state/rearms before waking outside locks.
            unsafe { self.timer.start(core_deadline(deadline)) }
                .expect("bounded timerfd deadline and current CPU are valid");
        }
        drop(inner);
        drop(programming);
        if fired {
            state.poll_rx.wake();
        }
        Ok((old_interval, old_remaining))
    }

    /// Returns `(interval, remaining)` after accounting for elapsed intervals.
    /// Does not consume unread expirations; newly observed readiness wakes pollers.
    ///
    /// # Panics
    /// Panics if internal state no longer matches the clock domain fixed at creation.
    pub fn gettime(&self) -> (TimeSpan, TimeSpan) {
        let state = self.timer.handler();
        let mut inner = state.inner.lock();
        let now = clock_now(state.clock_id);
        let was_zero = inner.expirations == 0;
        let has_expiration = inner.tick(now);
        let remaining = inner
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(now))
            .unwrap_or(TimeSpan::ZERO);
        let setting = (inner.interval, remaining);
        drop(inner);
        if was_zero && has_expiration {
            state.poll_rx.wake();
        }
        setting
    }

    fn read_expirations(&self) -> KResult<u64> {
        let state = self.timer.handler();
        let _programming = self.programming.lock();
        let mut inner = state.inner.lock();
        inner.tick(clock_now(state.clock_id));
        if inner.expirations == 0 {
            return Err(KError::WouldBlock);
        }
        let count = core::mem::take(&mut inner.expirations);
        if let Some(deadline) = inner.deadline {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: core_deadline bounds the deadline \
                          and the initialized task or timer callback supplies a valid CPU route"
            )]
            // SAFETY: file reads run in initialized task context. The owned
            // handler retains only independent state and obeys IRQ constraints.
            // The state lock serializes this rearm with callback accounting.
            unsafe { self.timer.start(core_deadline(deadline)) }
                .expect("bounded timerfd deadline and current CPU are valid");
        }
        Ok(count)
    }
}

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let state = self.timer.handler();
        let mut events = IoEvents::empty();
        let mut inner = state.inner.lock();
        inner.tick(clock_now(state.clock_id));
        events.set(IoEvents::IN, inner.expirations > 0);
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.timer.handler().poll_rx)?;
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
        block_on(poll_file_io(
            timerfd.as_ref(),
            IoEvents::IN,
            file.is_nonblocking(),
            || {
                let count = timerfd.read_expirations()?;
                buf[..size_of::<u64>()].copy_from_slice(&count.to_ne_bytes());
                Ok(size_of::<u64>())
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

unittest::path_mod_test!(timerfd_tests, "../ut-tests/timerfd.rs");
