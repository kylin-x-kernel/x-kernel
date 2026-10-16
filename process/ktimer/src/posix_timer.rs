// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX timer configuration, clock mapping, and overrun bookkeeping.

use kerrno::{KError, KResult};
use ksignal::Signo;
use ktime_types::{BoottimeInstant, MonotonicInstant, ProcessCpuInstant, SystemTime, TimeSpan};
use linux_raw_sys::general::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
};
use posix_types::k_sigval;

use crate::{
    Pid, Tid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::{ITimer, TimerInstant},
};

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
    /// Captures the raw bits of a user-provided `sigval`.
    ///
    /// The pointer view is read only to preserve the bit pattern; it is
    /// never dereferenced (see the `SAFETY` note at the read).
    pub fn from_raw(value: k_sigval) -> Self {
        Self {
            // SAFETY: `k_sigval` is an ABI carrier union. Reading the pointer
            // view preserves the raw user-provided bits regardless of which
            // union field userspace conceptually initialized.
            raw_bits: unsafe { value.sival_ptr as usize },
        }
    }

    pub(crate) fn from_timer_id(timer_id: i32) -> Self {
        Self {
            raw_bits: timer_id as usize,
        }
    }

    pub(crate) fn into_raw(self) -> k_sigval {
        k_sigval {
            sival_ptr: self.raw_bits as *mut _,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
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

    fn now(self, process_utime: TimeSpan, process_stime: TimeSpan) -> TimerInstant {
        match self {
            Self::Realtime => TimerInstant::Realtime(ktime::realtime()),
            Self::Monotonic => TimerInstant::Monotonic(khal::time::monotonic_time()),
            Self::Boottime => TimerInstant::Boottime(BoottimeInstant::from_span_since_origin(
                khal::time::monotonic_time().span_since_origin(),
            )),
            Self::ProcessCpu => {
                TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(
                    process_utime.saturating_add(process_stime),
                ))
            }
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

    fn runtime_deadline(self, deadline: TimerInstant) -> Option<MonotonicInstant> {
        match (self, deadline) {
            (Self::Realtime, TimerInstant::Realtime(deadline)) => {
                Some(ktime::realtime_deadline_to_monotonic(deadline))
            }
            (Self::Monotonic, TimerInstant::Monotonic(deadline)) => Some(deadline),
            (Self::Boottime, TimerInstant::Boottime(deadline)) => Some(
                MonotonicInstant::from_span_since_origin(deadline.span_since_origin()),
            ),
            (Self::ProcessCpu, TimerInstant::ProcessCpu(_)) => None,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: A POSIX timer's deadline is built \
                          from its own clock id, so the clock and deadline domains match"
            )]
            _ => panic!("POSIX timer deadline clock-domain mismatch"),
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
    clock: PosixTimerClock,
    spec: ITimer,
    notify: PosixTimerNotify,
    signal_seq: u32,
    pending_signal: bool,
    queued_overrun: u32,
    last_overrun: u32,
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
        signal_seq: u32,
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
            clock,
            spec: ITimer::default(),
            notify,
            signal_seq,
            pending_signal: false,
            queued_overrun: 0,
            last_overrun: 0,
        }
    }

    pub(crate) fn snapshot(
        &self,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> (TimeSpan, TimeSpan) {
        self.spec
            .snapshot(self.clock.now(process_utime, process_stime))
    }

    pub(crate) fn settime(
        &mut self,
        absolute: bool,
        interval: TimeSpan,
        value: TimeSpan,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let now = self.clock.now(process_utime, process_stime);
        let deadline = if value.is_zero() {
            None
        } else {
            Some(
                if absolute {
                    self.clock.absolute_deadline(value)
                } else {
                    now.checked_add(value)
                }
                .ok_or(KError::InvalidInput)?,
            )
        };
        let old = self.spec.set(now, interval, deadline);
        self.pending_signal = false;
        self.queued_overrun = 0;
        self.last_overrun = 0;
        Ok(old)
    }

    pub(crate) fn update(&mut self, process_utime: TimeSpan, process_stime: TimeSpan) -> usize {
        self.spec
            .update(self.clock.now(process_utime, process_stime))
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
            .and_then(|deadline| self.clock.runtime_deadline(deadline));
        self.spec
            .set_alarm(runtime_deadline, runtime_deadline.map(|_| owner_pid));
    }

    pub(crate) fn set_signal_seq(&mut self, signal_seq: u32) {
        self.signal_seq = signal_seq;
    }

    pub(crate) fn signal_seq(&self) -> u32 {
        self.signal_seq
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
    pub(crate) fn catch_up_queued_overrun(
        &mut self,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> bool {
        if !self.pending_signal {
            return false;
        }
        let expirations = self.update(process_utime, process_stime);
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
        if expirations == 0 {
            return None;
        }

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

        let overrun = expirations.saturating_sub(1);
        self.pending_signal = true;
        self.queued_overrun = overrun;
        // last_overrun is intentionally NOT written here.  It is frozen only
        // in on_signal_dequeued (POSIX: timer_getoverrun returns the overrun
        // count for the most recently *delivered* notification).

        let signal = TimerSignal::Posix {
            signo,
            timer_id,
            overrun: overrun.min(i32::MAX as u32) as i32,
            signal_seq: self.signal_seq,
            value: value.into_raw(),
        };
        Some(match target_tid {
            Some(tid) => TimerDelivery::Thread { tid, signal },
            None => TimerDelivery::Process(signal),
        })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/posix_timer.rs");
