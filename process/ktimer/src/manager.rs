// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned timer manager.

use alloc::{collections::BTreeMap, vec::Vec};

use kerrno::{KError, KResult};
use khal::time::{TimeValue, monotonic_time_nanos};
use posix_types::ITimerType;

use crate::{
    Pid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::{ITIMER_SIGNAL_CAPACITY, ITimer, timer_signal},
    posix_timer::{PosixTimer, PosixTimerClock, PosixTimerCreateNotify},
};

const ITIMER_REAL_INDEX: usize = ITimerType::Real as usize;
const ITIMER_VIRTUAL_INDEX: usize = ITimerType::Virtual as usize;
const ITIMER_PROF_INDEX: usize = ITimerType::Prof as usize;

/// A manager for process-shared signal-driven interval timers.
pub struct ProcessTimerManager {
    owner_pid: Pid,
    itimers: [ITimer; ITIMER_SIGNAL_CAPACITY],
    next_posix_timer_id: i32,
    next_posix_signal_seq: u32,
    posix_timers: BTreeMap<i32, PosixTimer>,
}

impl ProcessTimerManager {
    /// Creates a new [`ProcessTimerManager`].
    pub fn new(owner_pid: Pid) -> Self {
        Self {
            owner_pid,
            itimers: Default::default(),
            next_posix_timer_id: 1,
            next_posix_signal_seq: 1,
            posix_timers: BTreeMap::new(),
        }
    }

    /// Returns the current interval timer state.
    pub fn get_itimer(
        &self,
        timer_type: ITimerType,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> (TimeValue, TimeValue) {
        self.itimers[timer_type as usize].snapshot(Self::timer_clock_now_ns(
            timer_type,
            process_utime_ns,
            process_stime_ns,
        ))
    }

    /// Sets an interval timer and returns the previous state.
    pub fn set_itimer(
        &mut self,
        timer_type: ITimerType,
        interval_ns: usize,
        remaining_ns: usize,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> (TimeValue, TimeValue) {
        let now_ns = Self::timer_clock_now_ns(timer_type, process_utime_ns, process_stime_ns);
        let deadline_ns = remaining_ns
            .checked_add(now_ns)
            .filter(|_| remaining_ns != 0);
        let owner_pid = self.owner_pid;
        let timer = &mut self.itimers[timer_type as usize];
        let old = timer.set(now_ns, interval_ns, deadline_ns);
        if matches!(timer_type, ITimerType::Real) {
            Self::arm_itimer_real(timer, owner_pid);
        }
        old
    }

    /// Polls wall-clock-driven timers (ITIMER_REAL + alarm-backed POSIX timers).
    pub fn poll_wall_clock(&mut self) -> Vec<TimerDelivery> {
        let mut deliveries = Vec::new();
        let owner_pid = self.owner_pid;
        if self.itimers[ITIMER_REAL_INDEX].update(monotonic_time_nanos() as usize) > 0 {
            Self::arm_itimer_real(&mut self.itimers[ITIMER_REAL_INDEX], owner_pid);
            deliveries.push(TimerDelivery::Process(TimerSignal::Legacy {
                signo: timer_signal(ITimerType::Real),
            }));
        }

        for (timer_id, timer) in &mut self.posix_timers {
            if !timer.needs_alarm_task() {
                continue;
            }

            // Wall-clock timers read their own clock internally; CPU time args are unused.
            let expirations = timer.update(0, 0);
            if expirations == 0 {
                continue;
            }

            timer.arm_deadline(owner_pid);
            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }
        deliveries
    }

    /// Polls the CPU-based interval timers against aggregated process CPU time.
    pub fn poll_cpu_timers(
        &mut self,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> Vec<TimerDelivery> {
        let mut deliveries = Vec::new();

        if self.itimers[ITIMER_VIRTUAL_INDEX].update(process_utime_ns) > 0 {
            deliveries.push(TimerDelivery::Process(TimerSignal::Legacy {
                signo: timer_signal(ITimerType::Virtual),
            }));
        }
        if self.itimers[ITIMER_PROF_INDEX].update(process_utime_ns.saturating_add(process_stime_ns))
            > 0
        {
            deliveries.push(TimerDelivery::Process(TimerSignal::Legacy {
                signo: timer_signal(ITimerType::Prof),
            }));
        }

        for (timer_id, timer) in &mut self.posix_timers {
            if !timer.is_process_cpu() {
                continue;
            }

            let expirations = timer.update(process_utime_ns, process_stime_ns);
            if expirations == 0 {
                continue;
            }

            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }

        deliveries
    }

    pub fn create_posix_timer(
        &mut self,
        clock_id: i32,
        notify: PosixTimerCreateNotify,
    ) -> KResult<i32> {
        let Some(clock) = PosixTimerClock::from_clock_id(clock_id) else {
            return Err(KError::InvalidInput);
        };
        let timer_id = self.allocate_posix_timer_id()?;
        let signal_seq = self.allocate_posix_signal_seq();
        self.posix_timers.insert(
            timer_id,
            PosixTimer::new(clock, notify, timer_id, signal_seq),
        );
        Ok(timer_id)
    }

    pub fn get_posix_timer(
        &self,
        timer_id: i32,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> KResult<(TimeValue, TimeValue)> {
        self.posix_timers
            .get(&timer_id)
            .map(|timer| timer.snapshot(process_utime_ns, process_stime_ns))
            .ok_or(KError::InvalidInput)
    }

    pub fn set_posix_timer(
        &mut self,
        timer_id: i32,
        absolute: bool,
        interval_ns: usize,
        value_ns: usize,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> KResult<((TimeValue, TimeValue), Option<TimerDelivery>)> {
        let signal_seq = self.allocate_posix_signal_seq();
        let timer = self
            .posix_timers
            .get_mut(&timer_id)
            .ok_or(KError::InvalidInput)?;
        let owner_pid = self.owner_pid;
        timer.set_signal_seq(signal_seq);
        let old = timer.settime(
            absolute,
            interval_ns,
            value_ns,
            process_utime_ns,
            process_stime_ns,
        );
        timer.arm_deadline(owner_pid);
        // The timer may have already expired; update() advances the deadline
        // if so, requiring a second arm to register the new deadline.
        let expirations = timer.update(process_utime_ns, process_stime_ns);
        let delivery = timer.collect_delivery(timer_id, expirations);
        timer.arm_deadline(owner_pid);
        Ok((old, delivery))
    }

    pub fn delete_posix_timer(&mut self, timer_id: i32) -> KResult<()> {
        self.posix_timers
            .remove(&timer_id)
            .map(|_| ())
            .ok_or(KError::InvalidInput)
    }

    pub fn get_posix_timer_overrun(&self, timer_id: i32) -> KResult<i32> {
        self.posix_timers
            .get(&timer_id)
            .map(PosixTimer::overrun)
            .ok_or(KError::InvalidInput)
    }

    pub fn clear_posix_timers(&mut self) {
        self.posix_timers.clear();
    }

    pub fn on_timer_signal_dequeued(&mut self, timer_id: i32, signal_seq: u32) -> bool {
        if let Some(timer) = self.posix_timers.get_mut(&timer_id) {
            if timer.signal_seq() != signal_seq {
                return false;
            }
            timer.on_signal_dequeued();
            return true;
        }
        false
    }

    fn timer_clock_now_ns(
        timer_type: ITimerType,
        process_utime_ns: usize,
        process_stime_ns: usize,
    ) -> usize {
        match timer_type {
            ITimerType::Real => monotonic_time_nanos() as usize,
            ITimerType::Virtual => process_utime_ns,
            ITimerType::Prof => process_utime_ns.saturating_add(process_stime_ns),
        }
    }

    fn allocate_posix_timer_id(&mut self) -> KResult<i32> {
        let start = self.next_posix_timer_id;
        loop {
            let timer_id = self.next_posix_timer_id;
            self.next_posix_timer_id = if timer_id == i32::MAX {
                1
            } else {
                timer_id + 1
            };
            if !self.posix_timers.contains_key(&timer_id) {
                return Ok(timer_id);
            }
            if self.next_posix_timer_id == start {
                return Err(KError::InvalidInput);
            }
        }
    }

    fn allocate_posix_signal_seq(&mut self) -> u32 {
        let signal_seq = self.next_posix_signal_seq;
        self.next_posix_signal_seq = self.next_posix_signal_seq.checked_add(1).unwrap_or(1);
        signal_seq
    }

    fn arm_itimer_real(timer: &mut ITimer, owner_pid: Pid) {
        timer.set_alarm(timer.deadline_ns(), Some(owner_pid));
    }
}

#[cfg(unittest)]
mod tests {
    use ksignal::Signo;
    use linux_raw_sys::general::CLOCK_PROCESS_CPUTIME_ID;
    use posix_types::ITimerType;
    use unittest::def_test;

    use super::ProcessTimerManager;
    use crate::{
        TimerDelivery, TimerSignal,
        posix_timer::{PosixTimerCreateNotify, PosixTimerSigValue},
    };

    #[def_test]
    fn test_process_timer_manager_set_itimer_returns_previous_values() {
        let mut manager = ProcessTimerManager::new(1);

        let (old_interval, old_remained) = manager.set_itimer(ITimerType::Virtual, 11, 22, 0, 0);
        assert_eq!(old_interval.as_secs(), 0);
        assert_eq!(old_remained.as_secs(), 0);

        let (old_interval, old_remained) = manager.set_itimer(ITimerType::Virtual, 33, 44, 0, 0);
        assert_eq!(old_interval.subsec_nanos(), 11);
        assert_eq!(old_remained.subsec_nanos(), 22);

        let (interval, remained) = manager.get_itimer(ITimerType::Virtual, 0, 0);
        assert_eq!(interval.subsec_nanos(), 33);
        assert_eq!(remained.subsec_nanos(), 44);
    }

    #[def_test]
    fn test_posix_timer_signal_seq_invalidates_stale_signal() {
        let mut manager = ProcessTimerManager::new(1);
        let timer_id = manager
            .create_posix_timer(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGRTMIN,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap();
        let (_, delivery) = manager
            .set_posix_timer(timer_id, false, 0, 1, 0, 0)
            .unwrap();
        assert!(delivery.is_none());

        let delivery = manager
            .poll_cpu_timers(1, 0)
            .into_iter()
            .next()
            .expect("timer delivery");
        let stale_signal_seq = match delivery {
            TimerDelivery::Process(TimerSignal::Posix { signal_seq, .. }) => signal_seq,
            _ => panic!("expected a POSIX timer signal"),
        };

        let _ = manager
            .set_posix_timer(timer_id, false, 0, 10, 1, 0)
            .unwrap();
        assert!(!manager.on_timer_signal_dequeued(timer_id, stale_signal_seq));
    }
}
