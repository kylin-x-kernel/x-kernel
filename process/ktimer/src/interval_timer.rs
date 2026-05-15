// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy `setitimer` state and scheduling policy.

use khal::time::TimeValue;
use kprocess::Pid;
use ksignal::Signo;
use posix_types::ITimerType;

use crate::runtime;

pub(crate) const ITIMER_SIGNAL_CAPACITY: usize = 3;

pub(crate) fn timer_signal(timer_type: ITimerType) -> Signo {
    match timer_type {
        ITimerType::Real => Signo::SIGALRM,
        ITimerType::Virtual => Signo::SIGVTALRM,
        ITimerType::Prof => Signo::SIGPROF,
    }
}

#[derive(Default)]
pub(crate) struct ITimer {
    interval_ns: usize,
    deadline_ns: Option<usize>,
    runtime_deadline_ns: Option<usize>,
    alarm_pid: Option<Pid>,
}

impl ITimer {
    pub(crate) fn snapshot(&self, now_ns: usize) -> (TimeValue, TimeValue) {
        (
            TimeValue::from_nanos(self.interval_ns as u64),
            TimeValue::from_nanos(self.remaining_ns(now_ns) as u64),
        )
    }

    pub(crate) fn remaining_ns(&self, now_ns: usize) -> usize {
        self.deadline_ns
            .map(|deadline_ns| deadline_ns.saturating_sub(now_ns))
            .unwrap_or(0)
    }

    pub(crate) fn set(
        &mut self,
        now_ns: usize,
        interval_ns: usize,
        deadline_ns: Option<usize>,
    ) -> (TimeValue, TimeValue) {
        let old = self.snapshot(now_ns);

        *self = Self {
            interval_ns,
            deadline_ns,
            runtime_deadline_ns: None,
            alarm_pid: None,
        };
        old
    }

    pub(crate) fn update(&mut self, now_ns: usize) -> usize {
        let Some(deadline_ns) = self.deadline_ns else {
            return 0;
        };

        if now_ns < deadline_ns {
            return 0;
        }

        self.runtime_deadline_ns = None;
        self.alarm_pid = None;

        if self.interval_ns == 0 {
            self.deadline_ns = None;
            1
        } else {
            let overdue_ns = now_ns.saturating_sub(deadline_ns);
            let skipped_periods = overdue_ns
                .checked_div(self.interval_ns)
                .map(|periods| periods + 1)
                .expect("interval timers divide by a non-zero interval");
            let advance_ns = self.interval_ns.saturating_mul(skipped_periods);
            self.deadline_ns = deadline_ns.checked_add(advance_ns);
            skipped_periods
        }
    }

    pub(crate) fn deadline_ns(&self) -> Option<usize> {
        self.deadline_ns
    }

    pub(crate) fn set_alarm(&mut self, runtime_deadline_ns: Option<usize>, pid: Option<Pid>) {
        self.runtime_deadline_ns = runtime_deadline_ns;
        self.alarm_pid = pid;
        self.renew_alarm();
    }

    fn renew_alarm(&self) {
        if let (Some(runtime_deadline_ns), Some(pid)) = (self.runtime_deadline_ns, self.alarm_pid) {
            runtime::enqueue_alarm(runtime_deadline_ns, pid);
        }
    }
}

#[cfg(unittest)]
mod tests {
    use khal::time::TimeValue;
    use ksignal::Signo;
    use posix_types::ITimerType;
    use unittest::def_test;

    use super::{ITimer, timer_signal};

    #[def_test]
    fn test_timer_signal_mapping() {
        assert_eq!(timer_signal(ITimerType::Real), Signo::SIGALRM);
        assert_eq!(timer_signal(ITimerType::Virtual), Signo::SIGVTALRM);
        assert_eq!(timer_signal(ITimerType::Prof), Signo::SIGPROF);
    }

    #[def_test]
    fn test_time_value_from_nanos_basic() {
        let tv = TimeValue::from_nanos(0);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 0);
    }

    #[def_test]
    fn test_time_value_from_nanos_subsec() {
        let tv = TimeValue::from_nanos(500_000_000);
        assert_eq!(tv.as_secs(), 0);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_time_value_from_nanos_multi_sec() {
        let tv = TimeValue::from_nanos(2_500_000_000);
        assert_eq!(tv.as_secs(), 2);
        assert_eq!(tv.subsec_nanos(), 500_000_000);
    }

    #[def_test]
    fn test_itimer_update_zero_remained() {
        let mut timer = ITimer::default();
        assert_eq!(timer.update(100), 0);
    }

    #[def_test]
    fn test_itimer_update_counts_down_without_firing() {
        let mut timer = ITimer {
            interval_ns: 0,
            deadline_ns: Some(10),
            runtime_deadline_ns: None,
            alarm_pid: None,
        };
        assert_eq!(timer.update(3), 0);
        assert_eq!(timer.remaining_ns(3), 7);
    }

    #[def_test]
    fn test_itimer_update_fires_and_resets_to_interval() {
        let mut timer = ITimer {
            interval_ns: 0,
            deadline_ns: Some(5),
            runtime_deadline_ns: None,
            alarm_pid: None,
        };
        assert_eq!(timer.update(5), 1);
        assert_eq!(timer.remaining_ns(5), 0);
    }

    #[def_test]
    fn test_itimer_update_rearms_from_previous_deadline() {
        let mut timer = ITimer {
            interval_ns: 10,
            deadline_ns: Some(10),
            runtime_deadline_ns: None,
            alarm_pid: None,
        };

        assert_eq!(timer.update(35), 3);
        assert_eq!(timer.remaining_ns(35), 5);
    }

    #[def_test]
    fn test_itimer_update_periodic_rearms_from_previous_deadline() {
        let mut timer = ITimer {
            interval_ns: 5,
            deadline_ns: Some(3),
            runtime_deadline_ns: None,
            alarm_pid: None,
        };

        assert_eq!(timer.update(8), 2);
        assert_eq!(timer.remaining_ns(8), 5);
    }
}
