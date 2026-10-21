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

use alloc::{boxed::Box, sync::Arc};
use core::mem::size_of;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use kspin::SpinNoIrq;
use ksync::Mutex;
use ktask::future::block_on;
use ktime_types::{BoottimeInstant, MonotonicInstant, SystemTime, TimeSpan};
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsInode, poll_file_io};
/// Supported clock domains for a timerfd. Syscall adapters validate raw clock IDs
/// before constructing this type; relative REALTIME arming uses MONOTONIC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerFdClock {
    /// Wall-clock time, affected by wall-clock adjustments for absolute arming.
    Realtime,
    /// Monotonic elapsed time, excluding injected sleep.
    Monotonic,
    /// Elapsed time including explicitly injected sleep.
    Boottime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimerDeadline {
    Monotonic(MonotonicInstant),
    Realtime(SystemTime),
    Boottime(BoottimeInstant),
}

impl TimerDeadline {
    fn from_absolute(clock: TimerFdClock, value: TimeSpan) -> Option<Self> {
        match clock {
            TimerFdClock::Boottime => Some(Self::Boottime(
                BoottimeInstant::from_span_since_origin(value),
            )),
            TimerFdClock::Monotonic => Some(Self::Monotonic(
                MonotonicInstant::from_span_since_origin(value),
            )),
            TimerFdClock::Realtime => Some(Self::Realtime(SystemTime::from_unix_parts(
                i64::try_from(value.as_secs()).ok()?,
                value.subsec_nanos(),
            )?)),
        }
    }

    fn checked_add(self, span: TimeSpan) -> Option<Self> {
        match self {
            Self::Monotonic(instant) => instant.checked_add(span).map(Self::Monotonic),
            Self::Realtime(time) => time.checked_add(span).map(Self::Realtime),
            Self::Boottime(time) => time.checked_add(span).map(Self::Boottime),
        }
    }

    fn checked_duration_since(self, earlier: Self) -> Option<TimeSpan> {
        match (self, earlier) {
            (Self::Monotonic(now), Self::Monotonic(then)) => now.checked_duration_since(then),
            (Self::Realtime(now), Self::Realtime(then)) => now.duration_since(then).ok(),
            (Self::Boottime(now), Self::Boottime(then)) => now.checked_duration_since(then),
            _ => None,
        }
    }

    fn is_before(self, other: Self) -> bool {
        match (self, other) {
            (Self::Monotonic(lhs), Self::Monotonic(rhs)) => lhs < rhs,
            (Self::Realtime(lhs), Self::Realtime(rhs)) => lhs < rhs,
            (Self::Boottime(lhs), Self::Boottime(rhs)) => lhs < rhs,
            _ => false,
        }
    }

    fn now(self) -> Self {
        match self {
            Self::Monotonic(_) => Self::Monotonic(monotonic_time()),
            Self::Realtime(_) => Self::Realtime(ktime::realtime()),
            Self::Boottime(_) => Self::Boottime(ktime::boottime()),
        }
    }

    fn monotonic_at(self, reading: ktime::ClockReading) -> MonotonicInstant {
        let delay = match self {
            Self::Monotonic(deadline) => return deadline,
            Self::Realtime(deadline) => deadline
                .duration_since(reading.realtime)
                .unwrap_or(TimeSpan::ZERO),
            Self::Boottime(deadline) => deadline.saturating_duration_since(reading.boottime),
        };
        reading
            .monotonic
            .checked_add(delay)
            .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
    }

    fn now_at(self, reading: ktime::ClockReading) -> Self {
        match self {
            Self::Monotonic(_) => Self::Monotonic(reading.monotonic),
            Self::Realtime(_) => Self::Realtime(reading.realtime),
            Self::Boottime(_) => Self::Boottime(reading.boottime),
        }
    }
}

fn clock_now(clock: TimerFdClock) -> TimerDeadline {
    match clock {
        TimerFdClock::Boottime => TimerDeadline::Boottime(ktime::boottime()),
        TimerFdClock::Monotonic => TimerDeadline::Monotonic(monotonic_time()),
        TimerFdClock::Realtime => TimerDeadline::Realtime(ktime::realtime()),
    }
}

struct TimerFdInner {
    expirations: u64,
    interval: TimeSpan,
    deadline: Option<TimerDeadline>,
    armed_at: ktime::ClockChangeId,
    is_closed: bool,
}

impl TimerFdInner {
    fn now(&self) -> TimerDeadline {
        self.deadline.map_or(
            TimerDeadline::Monotonic(monotonic_time()),
            TimerDeadline::now,
        )
    }

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
                          the armed timer clock domain and now was checked not earlier"
            )]
            let overdue = now
                .checked_duration_since(deadline)
                .expect("timerfd accounting uses its armed clock domain");
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
    clock: TimerFdClock,
    inner: SpinNoIrq<TimerFdInner>,
    poll_rx: PollSet,
}

fn core_deadline(deadline: TimerDeadline) -> MonotonicInstant {
    let horizon = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
    deadline.monotonic_at(ktime::clock_reading()).min(horizon)
}

impl ktask::TimerHandler for TimerFdHandler {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        let mut inner = self.inner.lock();
        let has_expiration = {
            let now = inner.now();
            inner.tick(now)
        };
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
struct TimerFdRuntime {
    // Timer Drop drains the callback before destroying its embedded handler state.
    timer: ktask::Timer<TimerFdHandler>,
    // Settime/read serialize programming; callbacks/poll/gettime never take it.
    programming: Mutex<()>,
}

/// File ownership is separate from runtime delivery and deferred cleanup.
pub struct TimerFd {
    runtime: Arc<TimerFdRuntime>,
    clock_subscription: Option<(ktime::ClockObserverRegistration, Arc<TimerFdObserver>)>,
}

impl TimerFd {
    /// Creates a disarmed timer for a supported clock domain.
    ///
    /// MONOTONIC, BOOTTIME and absolute REALTIME retain their respective domains.
    /// Relative REALTIME arming uses MONOTONIC.
    /// The caller must initialize the timer runtime before programming it.
    /// Preparation allocates; subsequent programming reuses the timer storage.
    ///
    /// # Errors
    /// Returns InvalidInput outside sleepable preparation context, or
    /// OperationNotSupported if the normal work service needed by changing
    /// clocks is unavailable. MONOTONIC does not require that service.
    ///
    /// # Panics
    /// Panics if the timer provider violates its newly allocated exclusive
    /// storage invariant. Allocation failure follows the kernel allocator policy.
    pub fn new(clock: TimerFdClock) -> KResult<Arc<Self>> {
        let handler = TimerFdHandler {
            clock,
            inner: SpinNoIrq::new(TimerFdInner {
                deadline: None,
                interval: TimeSpan::ZERO,
                expirations: 0,
                armed_at: ktime::ClockChangeId::default(),
                is_closed: false,
            }),
            poll_rx: PollSet::new(),
        };
        let runtime = Arc::new(TimerFdRuntime {
            timer: ktask::Timer::new(handler),
            programming: Mutex::new(()),
        });
        let subscription = if matches!(clock, TimerFdClock::Realtime | TimerFdClock::Boottime) {
            let work_owner = runtime.clone();
            let callback: Box<dyn Fn() + Send + Sync> = Box::new(move || work_owner.refresh());
            let work = kworkng::WorkHandle::new(callback).map_err(|error| match error {
                kworkng::HandleCreateError::InvalidContext => KError::InvalidInput,
                kworkng::HandleCreateError::ServicesUnavailable => KError::OperationNotSupported,
            })?;
            {
                let observer = Arc::new(TimerFdObserver {
                    runtime: runtime.clone(),
                    work,
                });
                Some((ktime::register_clock_observer(observer.clone()), observer))
            }
        } else {
            None
        };
        Ok(Arc::new(Self {
            runtime,
            clock_subscription: subscription,
        }))
    }

    /// Creates the timerfd anonymous-inode file and captures `cred` as its open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from timer runtime preparation and anonymous-inode file creation.
    ///
    /// # Panics
    /// Has the timer-preparation invariant panic described by [`Self::new`].
    pub fn new_file(
        clock: TimerFdClock,
        open_flags: u32,
        cred: Arc<Cred>,
    ) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[timerfd]",
            Arc::new(TimerfdFops),
            Self::new(clock)?,
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
        let state = self.runtime.timer.handler();
        let now = if !absolute && state.clock == TimerFdClock::Realtime {
            TimerDeadline::Monotonic(monotonic_time())
        } else {
            clock_now(state.clock)
        };
        let deadline = if value.is_zero() {
            None
        } else {
            Some(
                if absolute {
                    TimerDeadline::from_absolute(state.clock, value)
                } else {
                    now.checked_add(value)
                }
                .ok_or(KError::InvalidInput)?,
            )
        };

        let programming = self.runtime.programming.lock();
        // Never wait with the callback's state lock held. Programming requests
        // serialize here; the callback only takes handler state and its core lock.
        self.runtime.timer.cancel_sync();
        let mut inner = state.inner.lock();
        let now = inner.now();
        inner.tick(now);
        let old_interval = inner.interval;
        let old_remaining = inner
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(now))
            .unwrap_or(TimeSpan::ZERO);
        // An observer can arrive late; samples committed before this arming
        // must never account against the replacement setting.
        inner.armed_at = ktime::clock_reading().change_id;
        inner.deadline = deadline;
        inner.interval = if deadline.is_some() {
            interval
        } else {
            TimeSpan::ZERO
        };
        inner.expirations = 0;
        let fired = {
            let now = inner.now();
            inner.tick(now)
        };
        if !fired && let Some(deadline) = inner.deadline {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: core_deadline bounds the deadline \
                          and the initialized task or timer callback supplies a valid CPU route"
            )]
            // SAFETY: settime runs in initialized task context. The FD retains
            // stable handler state; the handler preserves hard-IRQ context
            // and only updates locked state/rearms before waking outside locks.
            unsafe { self.runtime.timer.start(core_deadline(deadline)) }
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
    /// Panics if internal state no longer matches the armed clock domain.
    pub fn gettime(&self) -> (TimeSpan, TimeSpan) {
        let state = self.runtime.timer.handler();
        let mut inner = state.inner.lock();
        let now = inner.now();
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
        let state = self.runtime.timer.handler();
        let _programming = self.runtime.programming.lock();
        let mut inner = state.inner.lock();
        {
            let now = inner.now();
            inner.tick(now)
        };
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
            unsafe { self.runtime.timer.start(core_deadline(deadline)) }
                .expect("bounded timerfd deadline and current CPU are valid");
        }
        Ok(count)
    }
}

impl Pollable for TimerFd {
    fn poll(&self) -> IoEvents {
        let state = self.runtime.timer.handler();
        let mut inner = state.inner.lock();
        let now = inner.now();
        if inner.tick(now) {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.runtime.timer.handler().poll_rx)?;
        }
        Ok(())
    }
}

// The runtime field drops before WorkHandle. The prepared work capture keeps
// another strong runtime owner until its normal-task reclamation completes.
// Thus a final observer release on the clock writer's CPU cannot destroy the
// core Timer (and synchronously wait for delivery) in IRQ context.
struct TimerFdObserver {
    runtime: Arc<TimerFdRuntime>,
    work: kworkng::WorkHandle<Box<dyn Fn() + Send + Sync>>,
}
impl ktime::ClockObserver for TimerFdObserver {
    fn on_clock_update(&self, update: ktime::ClockUpdate) {
        let state = self.runtime.timer.handler();
        let mut inner = state.inner.lock();
        if inner.is_closed || !update.reading.change_id.is_after(inner.armed_at) {
            return;
        }
        let Some(deadline) = inner.deadline else {
            return;
        };
        let now = match deadline {
            TimerDeadline::Realtime(_) => TimerDeadline::Realtime(update.reading.realtime),
            TimerDeadline::Boottime(_) if update.includes_boottime => {
                TimerDeadline::Boottime(update.reading.boottime)
            }
            _ => return,
        };
        // Record the committed jump now, even if it is reversed before the
        // normal worker runs. This accounting does not program or wake users.
        inner.tick(now);
        drop(inner);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: observer retains an open WorkHandle \
                      bound to one permanent normal queue; implicit target reuses its running \
                      owner; AlreadyQueued is coalesced"
        )]
        // An unclaimed occurrence will sample after this publication, so it
        // already covers this refresh. A running occurrence permits a pending
        // successor; AlreadyQueued means that successor is present as well.
        // None does not force the submitting CPU: Queue::resolve_target first
        // reuses unfinished ownership, and accept_from_queue retries if that
        // owner changed. This handle never selects another queue/explicit CPU,
        // so cross-CPU submissions cannot produce OwnerConflict here.
        self.work
            .queue_default(None)
            .map(|_| ())
            .or_else(|error| {
                if error == kworkng::SubmitError::AlreadyQueued {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .expect("live timerfd runtime work is admissible");
    }
}
impl TimerFdRuntime {
    fn refresh(&self) {
        let _ = self.refresh_with(ktime::clock_reading);
    }

    fn refresh_with(&self, read: impl FnOnce() -> ktime::ClockReading) -> Option<MonotonicInstant> {
        let programming = self.programming.lock();
        // Normal worker context; never drain under the callback's state lock.
        self.timer.cancel_sync();
        let state = self.timer.handler();
        let mut inner = state.inner.lock();
        if inner.is_closed {
            return None;
        }
        let reading = read();
        let now = inner
            .deadline
            .map_or(TimerDeadline::Monotonic(reading.monotonic), |deadline| {
                deadline.now_at(reading)
            });
        let fired = inner.tick(now);
        let programmed = if fired {
            None
        } else {
            inner.deadline.map(|deadline| {
                let horizon =
                    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
                deadline.monotonic_at(reading).min(horizon)
            })
        };
        if let Some(deadline) = programmed {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: deadline is bounded and the \
                          prepared normal worker runs after CPU timer initialization"
            )]
            // SAFETY: prepared normal work runs on an initialized scheduler CPU;
            // runtime owns stable timer storage; handler is IRQ-compatible,
            // does not unwind/enable IRQs/reenter dispatch, and only locks state
            // then rearms or wakes after unlocking. start never waits for it.
            unsafe { self.timer.start(deadline) }
                .expect("bounded runtime timerfd deadline and initialized CPU");
        }
        drop(inner);
        drop(programming);
        if fired {
            state.poll_rx.wake();
        }
        programmed
    }
}
impl Drop for TimerFd {
    fn drop(&mut self) {
        {
            let mut inner = self.runtime.timer.handler().inner.lock();
            inner.is_closed = true;
            inner.deadline = None;
        }
        // Close first: late observer/worker callbacks cannot rearm. No state or
        // programming lock is held while draining IRQ delivery.
        drop(self.clock_subscription.take());
        self.runtime.timer.cancel_sync();
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
