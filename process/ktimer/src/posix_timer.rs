// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX timer configuration, clock mapping, and overrun bookkeeping.

use khal::time::{self, TimeValue, monotonic_time_nanos, wall_time};
use ksignal::Signo;
use linux_raw_sys::general::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME,
};
use posix_types::k_sigval;

use crate::{
    Pid, Tid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::ITimer,
};

#[derive(Clone, Copy)]
pub enum PosixTimerCreateNotify {
    None,
    Signal {
        signo: Signo,
        target_tid: Option<Tid>,
        value: PosixTimerSigValue,
    },
}

#[derive(Clone, Copy)]
pub enum PosixTimerSigValue {
    Explicit(TimerSigValue),
    TimerId,
}

#[derive(Clone, Copy)]
pub struct TimerSigValue {
    raw_bits: usize,
}

impl TimerSigValue {
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

    fn now_ns(self, process_utime_ns: u64, process_stime_ns: u64) -> u64 {
        match self {
            Self::Realtime => wall_time().as_nanos() as u64,
            Self::Monotonic | Self::Boottime => monotonic_time_nanos(),
            Self::ProcessCpu => process_utime_ns.saturating_add(process_stime_ns),
        }
    }

    fn runtime_deadline_ns(self, deadline_ns: u64) -> Option<usize> {
        match self {
            Self::Realtime => Some(
                deadline_ns
                    .saturating_sub(time::offset_ns())
                    .min(usize::MAX as u64) as usize,
            ),
            Self::Monotonic | Self::Boottime => Some(deadline_ns.min(usize::MAX as u64) as usize),
            Self::ProcessCpu => None,
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
        process_utime_ns: u64,
        process_stime_ns: u64,
    ) -> (TimeValue, TimeValue) {
        self.spec
            .snapshot(self.clock.now_ns(process_utime_ns, process_stime_ns))
    }

    pub(crate) fn settime(
        &mut self,
        absolute: bool,
        interval_ns: usize,
        value_ns: usize,
        process_utime_ns: u64,
        process_stime_ns: u64,
    ) -> (TimeValue, TimeValue) {
        let now_ns = self.clock.now_ns(process_utime_ns, process_stime_ns);
        let deadline_ns = if value_ns == 0 {
            None
        } else if absolute {
            Some(value_ns as u64)
        } else {
            (value_ns as u64).checked_add(now_ns)
        };
        let old = self.spec.set(now_ns, interval_ns as u64, deadline_ns);
        self.pending_signal = false;
        self.queued_overrun = 0;
        self.last_overrun = 0;
        old
    }

    pub(crate) fn update(&mut self, process_utime_ns: u64, process_stime_ns: u64) -> usize {
        self.spec
            .update(self.clock.now_ns(process_utime_ns, process_stime_ns))
    }

    pub(crate) fn needs_alarm_task(&self) -> bool {
        self.clock.needs_alarm_task()
    }

    pub(crate) fn is_process_cpu(&self) -> bool {
        self.clock == PosixTimerClock::ProcessCpu
    }

    pub(crate) fn arm_deadline(&mut self, owner_pid: Pid) {
        let runtime_deadline_ns = self
            .spec
            .deadline_ns()
            .and_then(|deadline_ns| self.clock.runtime_deadline_ns(deadline_ns));
        self.spec
            .set_alarm(runtime_deadline_ns, runtime_deadline_ns.map(|_| owner_pid));
    }

    pub(crate) fn set_signal_seq(&mut self, signal_seq: u32) {
        self.signal_seq = signal_seq;
    }

    pub(crate) fn signal_seq(&self) -> u32 {
        self.signal_seq
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

        let expirations = expirations as u32;
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
