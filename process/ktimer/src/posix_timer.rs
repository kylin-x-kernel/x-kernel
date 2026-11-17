// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX timer configuration, clock mapping, and overrun bookkeeping.

use alloc::sync::Weak;

use kerrno::{KError, KResult};
use ksignal::{PreparedTimerRecord, Signo, TimerSignalOrigin, TimerSignalTarget};
use ktime_types::{BoottimeInstant, MonotonicInstant, ProcessCpuInstant, SystemTime, TimeSpan};
use linux_raw_sys::general::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
};

use crate::{
    Pid, Tid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::{ITimer, TimerInstant},
};

/// Validated configuration consumed by timer admission.
pub struct PosixTimerConfig {
    clock: PosixTimerClock,
    notify: PosixTimerCreateNotify,
    origin: Weak<dyn TimerSignalOrigin>,
    target: TimerSignalTarget,
    record: Option<PreparedTimerRecord>,
}

/// Rejected admission retaining the unconsumed timer configuration.
///
/// Move this value out of the timer-manager critical section before releasing
/// it. Rejection must not destroy resources prepared outside that section.
pub struct PosixTimerCreateError {
    error: KError,
    config: PosixTimerConfig,
}

impl PosixTimerCreateError {
    pub(crate) fn new(error: KError, config: PosixTimerConfig) -> Self {
        Self { error, config }
    }

    /// Recovers the error and configuration for unlocked release or retry.
    pub fn into_parts(self) -> (KError, PosixTimerConfig) {
        (self.error, self.config)
    }
}

impl core::fmt::Debug for PosixTimerCreateError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PosixTimerCreateError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl PosixTimerConfig {
    /// Validates the clock before entering the timer-manager critical section.
    ///
    /// This stage reserves notification storage and publishes no timer identity. The
    /// syscall layer must already have validated the notification target.
    /// Call in a sleepable, reclaim-safe task context without timer or signal
    /// locks, IRQ/preemption exclusion or locks required by reclaim. SIGEV_NONE
    /// performs no notification allocation. Nested timer-map/alarm allocations
    /// are not covered by this reservation.
    ///
    /// `origin` must name the runtime admitting this timer. Weak ownership
    /// avoids a pending-queue/runtime cycle and allocates no reference storage.
    /// `target` must be the already captured signal destination selected by the
    /// notification request. Numeric routing fields are descriptive only for
    /// POSIX dispatch. SIGEV_NONE retains no signal record and never sends.
    ///
    /// # Errors
    /// Returns InvalidInput for an unsupported clock, or NoMemory if reservation fails.
    pub fn new(
        clock_id: i32,
        notify: PosixTimerCreateNotify,
        origin: Weak<dyn TimerSignalOrigin>,
        target: TimerSignalTarget,
    ) -> KResult<Self> {
        Self::new_with_reservation(clock_id, notify, origin, target, PreparedTimerRecord::new)
    }

    fn new_with_reservation(
        clock_id: i32,
        notify: PosixTimerCreateNotify,
        origin: Weak<dyn TimerSignalOrigin>,
        target: TimerSignalTarget,
        reserve: impl FnOnce(Weak<dyn TimerSignalOrigin>) -> KResult<PreparedTimerRecord>,
    ) -> KResult<Self> {
        let clock = PosixTimerClock::from_clock_id(clock_id).ok_or(KError::InvalidInput)?;
        let record = match notify {
            PosixTimerCreateNotify::None => None,
            PosixTimerCreateNotify::Signal { .. } => Some(reserve(origin.clone())?),
        };
        Ok(Self {
            clock,
            notify,
            origin,
            target,
            record,
        })
    }

    pub(crate) fn into_timer(self, timer_id: i32, signal_seq: u64) -> PosixTimer {
        PosixTimer::new(
            self.clock,
            self.notify,
            timer_id,
            signal_seq,
            self.origin,
            self.target,
            self.record,
        )
    }
}

/// Notification configuration requested at `timer_create`.
#[derive(Clone, Copy)]
pub enum PosixTimerCreateNotify {
    /// `SIGEV_NONE`: track expirations without notifying anyone.
    None,
    /// `SIGEV_SIGNAL` (or `SIGEV_THREAD_ID` when `target_tid` is set).
    Signal {
        /// Signal number to deliver on expiration.
        signo: Signo,
        /// Thread-directed target for `SIGEV_THREAD_ID`; process-directed
        /// when `None`.
        target_tid: Option<Tid>,
        /// `sigval` payload source.
        value: PosixTimerSigValue,
    },
}

/// Source of the `sigval` payload carried by POSIX timer signals.
#[derive(Clone, Copy)]
pub enum PosixTimerSigValue {
    /// Use the exact `union sigval` bits provided by the user.
    Explicit(TimerSigValue),
    /// Use the kernel timer ID as the payload (POSIX default).
    TimerId,
}

/// Opaque `union sigval` carrier preserving the raw ABI bits.
#[derive(Clone, Copy)]
pub struct TimerSigValue {
    raw_bits: usize,
}

impl TimerSigValue {
    /// Wraps the complete pointer-sized bits of a user-provided `sigval`.
    pub fn from_raw_bits(raw_bits: usize) -> Self {
        Self { raw_bits }
    }

    pub(crate) fn from_timer_id(timer_id: i32) -> Self {
        Self {
            raw_bits: timer_id as usize,
        }
    }

    pub(crate) fn raw_bits(self) -> usize {
        self.raw_bits
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum PosixTimerClock {
    Realtime,
    Monotonic,
    Boottime,
    ProcessCpu,
}

impl PosixTimerClock {
    pub(crate) fn from_clock_id(clock_id: i32) -> Option<Self> {
        match clock_id as u32 {
            CLOCK_REALTIME => Some(Self::Realtime),
            CLOCK_MONOTONIC => Some(Self::Monotonic),
            CLOCK_BOOTTIME => Some(Self::Boottime),
            CLOCK_PROCESS_CPUTIME_ID => Some(Self::ProcessCpu),
            _ => None,
        }
    }

    fn now(self, process_sched_runtime: TimeSpan) -> TimerInstant {
        match self {
            Self::Realtime => TimerInstant::Realtime(ktime::realtime()),
            Self::Monotonic => TimerInstant::Monotonic(khal::time::monotonic_time()),
            Self::Boottime => TimerInstant::Boottime(ktime::boottime()),
            Self::ProcessCpu => TimerInstant::ProcessCpu(
                ProcessCpuInstant::from_span_since_origin(process_sched_runtime),
            ),
        }
    }

    fn absolute_deadline(self, value: TimeSpan) -> Option<TimerInstant> {
        match self {
            Self::Realtime => Some(TimerInstant::Realtime(SystemTime::from_unix_parts(
                i64::try_from(value.as_secs()).ok()?,
                value.subsec_nanos(),
            )?)),
            Self::Monotonic => Some(TimerInstant::Monotonic(
                MonotonicInstant::from_span_since_origin(value),
            )),
            Self::Boottime => Some(TimerInstant::Boottime(
                BoottimeInstant::from_span_since_origin(value),
            )),
            Self::ProcessCpu => Some(TimerInstant::ProcessCpu(
                ProcessCpuInstant::from_span_since_origin(value),
            )),
        }
    }

    fn needs_alarm_task(self) -> bool {
        !matches!(self, Self::ProcessCpu)
    }
}

#[derive(Clone, Copy)]
enum PosixTimerNotify {
    None,
    Signal {
        signo: Signo,
        target_tid: Option<Tid>,
        value: TimerSigValue,
    },
}

pub(crate) struct PosixTimer {
    _origin: Weak<dyn TimerSignalOrigin>,
    target: TimerSignalTarget,
    record: Option<PreparedTimerRecord>,
    clock: PosixTimerClock,
    armed_clock: PosixTimerClock,
    spec: ITimer,
    notify: PosixTimerNotify,
    signal_seq: u64,
    pending_signal: bool,
    queued_overrun: u32,
    last_overrun: u32,
    deferred_expirations: u32,
}

/// Narrows a collected expiration count for overrun accounting.
///
/// Overrun counters saturate instead of wrapping: a truncated count bypasses
/// the `i32::MAX` clamp in [`PosixTimer::overrun`] and reports a number far
/// below the real one (`2^32` expirations would even report zero). Linux keeps
/// a wider `it_overrun` internally and clamps only at the ABI boundary, so
/// saturating here preserves that observable behaviour.
fn saturating_overrun(expirations: usize) -> u32 {
    u32::try_from(expirations).unwrap_or(u32::MAX)
}

impl PosixTimer {
    pub(crate) fn new(
        clock: PosixTimerClock,
        notify: PosixTimerCreateNotify,
        timer_id: i32,
        signal_seq: u64,
        origin: Weak<dyn TimerSignalOrigin>,
        target: TimerSignalTarget,
        record: Option<PreparedTimerRecord>,
    ) -> Self {
        let notify = match notify {
            PosixTimerCreateNotify::None => PosixTimerNotify::None,
            PosixTimerCreateNotify::Signal {
                signo,
                target_tid,
                value,
            } => PosixTimerNotify::Signal {
                signo,
                target_tid,
                value: match value {
                    PosixTimerSigValue::Explicit(value) => value,
                    PosixTimerSigValue::TimerId => TimerSigValue::from_timer_id(timer_id),
                },
            },
        };

        Self {
            _origin: origin,
            target,
            record,
            clock,
            armed_clock: clock,
            spec: ITimer::default(),
            notify,
            signal_seq,
            pending_signal: false,
            queued_overrun: 0,
            last_overrun: 0,
            deferred_expirations: 0,
        }
    }

    pub(crate) fn snapshot(&self, process_sched_runtime: TimeSpan) -> (TimeSpan, TimeSpan) {
        self.spec
            .snapshot(self.armed_clock.now(process_sched_runtime))
    }

    pub(crate) fn settime(
        &mut self,
        absolute: bool,
        interval: TimeSpan,
        value: TimeSpan,
        process_sched_runtime: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let armed_clock = if !absolute && self.clock == PosixTimerClock::Realtime {
            PosixTimerClock::Monotonic
        } else {
            self.clock
        };
        let now = armed_clock.now(process_sched_runtime);
        let deadline = if value.is_zero() {
            None
        } else {
            Some(
                if absolute {
                    armed_clock.absolute_deadline(value)
                } else {
                    now.checked_add(value)
                }
                .ok_or(KError::InvalidInput)?,
            )
        };
        let old = self.snapshot(process_sched_runtime);
        // Snapshot in the old domain before replacing the armed clock.
        self.spec.replace(interval, deadline);
        self.armed_clock = armed_clock;
        self.pending_signal = false;
        self.queued_overrun = 0;
        self.last_overrun = 0;
        self.deferred_expirations = 0;
        Ok(old)
    }

    pub(crate) fn update(&mut self, process_sched_runtime: TimeSpan) -> usize {
        self.spec
            .update(self.armed_clock.now(process_sched_runtime))
    }

    pub(crate) fn needs_alarm_task(&self) -> bool {
        self.clock.needs_alarm_task()
    }

    pub(crate) fn is_process_cpu(&self) -> bool {
        self.clock == PosixTimerClock::ProcessCpu
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.spec.deadline().is_some()
    }

    pub(crate) fn arm_deadline(&mut self, owner_pid: Pid) {
        let runtime_deadline = self
            .spec
            .deadline()
            .filter(|_| self.armed_clock.needs_alarm_task());
        self.spec
            .set_alarm(runtime_deadline, runtime_deadline.map(|_| owner_pid));
    }

    pub(crate) fn set_signal_seq(&mut self, signal_seq: u64) {
        self.signal_seq = signal_seq;
    }

    pub(crate) fn invalidate_notifications(&self) {
        if let Some(record) = &self.record {
            record.invalidate_through(self.signal_seq);
        }
    }

    pub(crate) fn cancel_notifications(&self) -> Option<ksignal::TimerSignalCancellation> {
        self.record
            .as_ref()
            .map(|record| record.invalidate(&self.target, self.signal_seq))
    }

    pub(crate) fn signal_seq(&self) -> u64 {
        self.signal_seq
    }

    pub(crate) fn owns_record(&self, record: &PreparedTimerRecord) -> bool {
        self.record
            .as_ref()
            .is_some_and(|owned| owned.same_storage(record))
    }

    pub(crate) fn pending_wake_target(&self) -> Option<(TimerSignalTarget, Signo)> {
        let PosixTimerNotify::Signal { signo, .. } = self.notify else {
            return None;
        };
        self.pending_signal.then(|| (self.target.clone(), signo))
    }

    /// Folds intervals that are already due into the pending notification's
    /// overrun count.
    ///
    /// The notification is dequeued for delivery on the signal path while its
    /// expirations are collected by the alarm task, so an interval can elapse
    /// between the last collection and the delivery. POSIX attributes those
    /// expirations to the most recently delivered notification; leaving them
    /// unaccounted would let the next collection emit a second signal for the
    /// same timer instead of counting an overrun. Without a pending
    /// notification there is nothing to attribute — those expirations belong
    /// to the next notification.
    ///
    /// # Returns
    ///
    /// `true` when the interval schedule advanced, i.e. the caller must re-arm
    /// the alarm for the new deadline: a redundant re-arm re-enqueues an
    /// identical entry in the global alarm heap, which is not de-duplicated.
    pub(crate) fn catch_up_queued_overrun(&mut self, process_sched_runtime: TimeSpan) -> bool {
        if !self.pending_signal {
            return false;
        }
        let expirations = self.update(process_sched_runtime);
        if expirations == 0 {
            return false;
        }
        self.queued_overrun = self
            .queued_overrun
            .saturating_add(saturating_overrun(expirations));
        true
    }

    pub(crate) fn on_signal_dequeued(&mut self) {
        if !self.pending_signal {
            return;
        }
        self.pending_signal = false;
        self.last_overrun = self.queued_overrun;
        self.queued_overrun = 0;
    }

    pub(crate) fn overrun(&self) -> i32 {
        self.last_overrun.min(i32::MAX as u32) as i32
    }

    /// Test observation of the registered runtime alarm deadline.
    #[cfg(unittest)]
    pub(crate) fn alarm_deadline(&self) -> Option<MonotonicInstant> {
        self.spec.alarm_deadline()
    }

    pub(crate) fn collect_delivery(
        &mut self,
        timer_id: i32,
        expirations: usize,
    ) -> Option<TimerDelivery> {
        let PosixTimerNotify::Signal {
            signo,
            target_tid,
            value,
        } = self.notify
        else {
            return None;
        };

        let expirations = saturating_overrun(expirations);
        if self.pending_signal {
            self.queued_overrun = self.queued_overrun.saturating_add(expirations);
            // Do NOT update last_overrun here: timer_getoverrun must return the
            // overrun frozen at the most recent signal *delivery* (dequeue),
            // not the live pending count.  Writing last_overrun here would let
            // a subsequent collect_delivery (pending_signal == false) overwrite
            // the dequeued value with a fresh `expirations - 1` before the
            // caller reads it — a race that produces overrun == 0 when the
            // alarm task fires at sub-tick precision (1 expiration per fire).
            return None;
        }

        self.deferred_expirations = self.deferred_expirations.saturating_add(expirations);
        if self.deferred_expirations == 0 {
            return None;
        }
        let overrun = self.deferred_expirations.saturating_sub(1);
        let notification = self.record.as_ref()?.prepare(
            signo,
            timer_id,
            overrun.min(i32::MAX as u32) as i32,
            value.raw_bits(),
            self.signal_seq,
            self.spec.is_periodic(),
        )?;
        self.deferred_expirations = 0;
        self.pending_signal = true;
        self.queued_overrun = overrun;
        // last_overrun is intentionally NOT written here.  It is frozen only
        // in on_signal_dequeued (POSIX: timer_getoverrun returns the overrun
        // count for the most recently *delivered* notification).

        let signal = TimerSignal::Posix {
            target: self.target.clone(),
            notification,
        };
        Some(match target_tid {
            Some(tid) => TimerDelivery::Thread { tid, signal },
            None => TimerDelivery::Process(signal),
        })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/posix_timer.rs");
