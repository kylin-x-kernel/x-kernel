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

#[cfg(unittest)]
mod tests {
    use ksignal::Signo;
    use ktime_types::{ProcessCpuInstant, TimeSpan};
    use posix_types::ITimerType;
    use unittest::def_test;

    use super::{ITimer, TimerInstant, timer_signal};

    fn cpu_instant(nanos: u64) -> TimerInstant {
        TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(
            TimeSpan::from_nanos(nanos),
        ))
    }

    #[def_test]
    fn test_timer_signal_mapping() {
        assert_eq!(timer_signal(ITimerType::Real), Signo::SIGALRM);
        assert_eq!(timer_signal(ITimerType::Virtual), Signo::SIGVTALRM);
        assert_eq!(timer_signal(ITimerType::Prof), Signo::SIGPROF);
    }

    #[def_test]
    fn test_time_value_from_nanos_basic() {
        let tv = TimeSpan::from_nanos(0);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_time_value_from_nanos_subsec() {
        let tv = TimeSpan::from_nanos(500_000_000);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_time_value_from_nanos_multi_sec() {
        let tv = TimeSpan::from_nanos(2_500_000_000);
        assert_eq!(tv.as_secs(), 2);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_itimer_update_zero_remained() {
        let mut timer = ITimer::default();
        assert_eq!(timer.update(cpu_instant(100)), 0);
    }

    #[def_test]
    fn test_itimer_update_counts_down_without_firing() {
        let mut timer = ITimer {
            interval: TimeSpan::ZERO,
            deadline: Some(cpu_instant(10)),
            runtime_deadline: None,
            alarm_pid: None,
        };
        assert_eq!(timer.update(cpu_instant(3)), 0);
        assert_eq!(timer.remaining(cpu_instant(3)), TimeSpan::from_nanos(7));
    }

    #[def_test]
    fn test_itimer_update_fires_and_resets_to_interval() {
        let mut timer = ITimer {
            interval: TimeSpan::ZERO,
            deadline: Some(cpu_instant(5)),
            runtime_deadline: None,
            alarm_pid: None,
        };
        assert_eq!(timer.update(cpu_instant(5)), 1);
        assert_eq!(timer.remaining(cpu_instant(5)), TimeSpan::ZERO);
    }

    #[def_test]
    fn test_itimer_update_rearms_from_previous_deadline() {
        let mut timer = ITimer {
            interval: TimeSpan::from_nanos(10),
            deadline: Some(cpu_instant(10)),
            runtime_deadline: None,
            alarm_pid: None,
        };

        assert_eq!(timer.update(cpu_instant(35)), 3);
        assert_eq!(timer.remaining(cpu_instant(35)), TimeSpan::from_nanos(5));
    }

    #[def_test]
    fn test_itimer_update_periodic_rearms_from_previous_deadline() {
        let mut timer = ITimer {
            interval: TimeSpan::from_nanos(5),
            deadline: Some(cpu_instant(3)),
            runtime_deadline: None,
            alarm_pid: None,
        };

        assert_eq!(timer.update(cpu_instant(8)), 2);
        assert_eq!(timer.remaining(cpu_instant(8)), TimeSpan::from_nanos(5));
    }
}
