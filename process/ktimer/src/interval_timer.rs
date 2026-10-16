// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy `setitimer` state and scheduling policy.

use ksignal::Signo;
use ktime_types::{BoottimeInstant, MonotonicInstant, ProcessCpuInstant, SystemTime, TimeSpan};
use posix_types::ITimerType;

use crate::{Pid, runtime};

pub(crate) const ITIMER_SIGNAL_CAPACITY: usize = 3;

pub(crate) fn timer_signal(timer_type: ITimerType) -> Signo {
    match timer_type {
        ITimerType::Real => Signo::SIGALRM,
        ITimerType::Virtual => Signo::SIGVTALRM,
        ITimerType::Prof => Signo::SIGPROF,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimerInstant {
    Realtime(SystemTime),
    Monotonic(MonotonicInstant),
    Boottime(BoottimeInstant),
    ProcessCpu(ProcessCpuInstant),
}

impl TimerInstant {
    pub(crate) fn checked_add(self, duration: TimeSpan) -> Option<Self> {
        match self {
            Self::Realtime(now) => now.checked_add(duration).map(Self::Realtime),
            Self::Monotonic(now) => now.checked_add(duration).map(Self::Monotonic),
            Self::Boottime(now) => now.checked_add(duration).map(Self::Boottime),
            Self::ProcessCpu(now) => now.checked_add(duration).map(Self::ProcessCpu),
        }
    }

    pub(crate) fn saturating_duration_since(self, earlier: Self) -> TimeSpan {
        match (self, earlier) {
            (Self::Realtime(now), Self::Realtime(earlier)) => {
                now.duration_since(earlier).unwrap_or(TimeSpan::ZERO)
            }
            (Self::Monotonic(now), Self::Monotonic(earlier)) => {
                now.saturating_duration_since(earlier)
            }
            (Self::Boottime(now), Self::Boottime(earlier)) => {
                now.saturating_duration_since(earlier)
            }
            (Self::ProcessCpu(now), Self::ProcessCpu(earlier)) => {
                now.saturating_duration_since(earlier)
            }
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: Callers compare instants read \
                          from the same timer's clock, so both sides share one clock domain"
            )]
            (now, earlier) => {
                panic!("timer instant clock-domain mismatch: {now:?} vs {earlier:?}")
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ITimer {
    interval: TimeSpan,
    deadline: Option<TimerInstant>,
    runtime_deadline: Option<MonotonicInstant>,
    alarm_pid: Option<Pid>,
}

impl ITimer {
    pub(crate) fn snapshot(&self, now: TimerInstant) -> (TimeSpan, TimeSpan) {
        (self.interval, self.remaining(now))
    }

    pub(crate) fn remaining(&self, now: TimerInstant) -> TimeSpan {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(TimeSpan::ZERO)
    }

    pub(crate) fn set(
        &mut self,
        now: TimerInstant,
        interval: TimeSpan,
        deadline: Option<TimerInstant>,
    ) -> (TimeSpan, TimeSpan) {
        let old = self.snapshot(now);

        *self = Self {
            interval,
            deadline,
            runtime_deadline: None,
            alarm_pid: None,
        };
        old
    }

    pub(crate) fn update(&mut self, now: TimerInstant) -> usize {
        let Some(deadline) = self.deadline else {
            return 0;
        };

        let overdue = now.saturating_duration_since(deadline);
        if overdue.is_zero() && now != deadline {
            return 0;
        }

        self.runtime_deadline = None;
        self.alarm_pid = None;

        if self.interval.is_zero() {
            self.deadline = None;
            1
        } else {
            let skipped_periods = overdue.as_nanos() / self.interval.as_nanos() + 1;
            let advance =
                TimeSpan::try_from_nanos(self.interval.as_nanos().saturating_mul(skipped_periods))
                    .unwrap_or(TimeSpan::MAX);
            self.deadline = deadline.checked_add(advance);
            skipped_periods.min(usize::MAX as u128) as usize
        }
    }

    pub(crate) fn deadline(&self) -> Option<TimerInstant> {
        self.deadline
    }

    /// Test observation of the runtime alarm registration.
    #[cfg(unittest)]
    pub(crate) fn alarm_deadline(&self) -> Option<MonotonicInstant> {
        self.runtime_deadline
    }

    pub(crate) fn set_alarm(
        &mut self,
        runtime_deadline: Option<MonotonicInstant>,
        pid: Option<Pid>,
    ) {
        self.runtime_deadline = runtime_deadline;
        self.alarm_pid = pid;
        self.renew_alarm();
    }

    fn renew_alarm(&self) {
        if let (Some(runtime_deadline), Some(pid)) = (self.runtime_deadline, self.alarm_pid) {
            runtime::enqueue_alarm(runtime_deadline, pid);
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/interval_timer.rs");
