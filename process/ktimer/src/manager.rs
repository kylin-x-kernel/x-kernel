// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned timer manager.

use alloc::{collections::BTreeMap, vec::Vec};

use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use ktime_types::{ProcessCpuInstant, TimeSpan};
use posix_types::ITimerType;

use crate::{
    Pid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::{ITIMER_SIGNAL_CAPACITY, ITimer, TimerInstant, timer_signal},
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
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> (TimeSpan, TimeSpan) {
        self.itimers[timer_type as usize].snapshot(Self::timer_clock_now(
            timer_type,
            process_utime,
            process_stime,
        ))
    }

    /// Sets an interval timer and returns the previous state.
    pub fn set_itimer(
        &mut self,
        timer_type: ITimerType,
        interval: TimeSpan,
        remaining: TimeSpan,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> (TimeSpan, TimeSpan) {
        let now = Self::timer_clock_now(timer_type, process_utime, process_stime);
        let deadline = if remaining.is_zero() {
            None
        } else {
            now.checked_add(remaining)
        };
        let owner_pid = self.owner_pid;
        let timer = &mut self.itimers[timer_type as usize];
        let old = timer.set(now, interval, deadline);
        if matches!(timer_type, ITimerType::Real) {
            Self::arm_itimer_real(timer, owner_pid);
        }
        old
    }

    /// Polls wall-clock-driven timers (ITIMER_REAL + alarm-backed POSIX timers).
    pub fn poll_wall_clock(&mut self) -> Vec<TimerDelivery> {
        let mut deliveries = Vec::new();
        let owner_pid = self.owner_pid;
        if self.itimers[ITIMER_REAL_INDEX].update(TimerInstant::Monotonic(monotonic_time())) > 0 {
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
            let expirations = timer.update(TimeSpan::ZERO, TimeSpan::ZERO);
            // A consumed queue entry is only a hint. If the wall clock moved
            // back before update, renew its scheduling without duplicating a
            // still-queued ticket. The ticket also deduplicates repeated polls.
            timer.arm_deadline(owner_pid);
            if expirations == 0 {
                continue;
            }

            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }
        deliveries
    }

    /// Polls the CPU-based interval timers against aggregated process CPU time.
    ///
    /// The iterated sets must stay in sync with `any_cpu_timer_armed`; see
    /// the sync note there.
    pub fn poll_cpu_timers(
        &mut self,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> Vec<TimerDelivery> {
        let mut deliveries = Vec::new();

        if self.itimers[ITIMER_VIRTUAL_INDEX].update(TimerInstant::ProcessCpu(
            ProcessCpuInstant::from_span_since_origin(process_utime),
        )) > 0
        {
            deliveries.push(TimerDelivery::Process(TimerSignal::Legacy {
                signo: timer_signal(ITimerType::Virtual),
            }));
        }
        if self.itimers[ITIMER_PROF_INDEX].update(TimerInstant::ProcessCpu(
            ProcessCpuInstant::from_span_since_origin(process_utime.saturating_add(process_stime)),
        )) > 0
        {
            deliveries.push(TimerDelivery::Process(TimerSignal::Legacy {
                signo: timer_signal(ITimerType::Prof),
            }));
        }

        for (timer_id, timer) in &mut self.posix_timers {
            if !timer.is_process_cpu() {
                continue;
            }

            let expirations = timer.update(process_utime, process_stime);
            if expirations == 0 {
                continue;
            }

            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }

        deliveries
    }

    /// Returns whether any CPU-time-driven timer is currently armed.
    ///
    /// Used as the fast-path gate for per-trap CPU-timer polling: false means
    /// `poll_cpu_timers` cannot produce a delivery, so callers can skip the
    /// process-wide CPU-time sampling entirely. Wall-clock timers
    /// (ITIMER_REAL, monotonic POSIX clocks) never arm this gate.
    pub fn any_cpu_timer_armed(&self) -> bool {
        if self.itimers[ITIMER_VIRTUAL_INDEX].deadline().is_some()
            || self.itimers[ITIMER_PROF_INDEX].deadline().is_some()
        {
            return true;
        }
        self.posix_timers
            .values()
            .any(|timer| timer.is_process_cpu() && timer.is_armed())
    }

    /// Creates a POSIX timer with the given clock and notification policy.
    ///
    /// # Returns
    ///
    /// The kernel timer ID allocated for the new timer.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` (`InvalidInput`) when `clock_id` is not a supported
    /// POSIX timer clock or when the ID space is exhausted.
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

    /// Returns the current `(interval, remaining)` state of a POSIX timer.
    ///
    /// `process_utime`/`process_stime` must be the owner process's current
    /// CPU-time totals (needed by `CLOCK_PROCESS_CPUTIME_ID` timers).
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` does not name a live timer.
    pub fn get_posix_timer(
        &self,
        timer_id: i32,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        self.posix_timers
            .get(&timer_id)
            .map(|timer| timer.snapshot(process_utime, process_stime))
            .ok_or(KError::InvalidInput)
    }

    /// Arms or re-arms a POSIX timer and returns its previous state plus any
    /// delivery that already expired at `settime`.
    ///
    /// A zero `value` disarms the timer. Absolute values select the
    /// `TIMER_ABSTIME` deadline interpretation. `signal_seq` is bumped so
    /// in-flight notifications from the previous arming become stale.
    ///
    /// # Returns
    ///
    /// The previous `(interval, remaining)` state and the immediate delivery,
    /// if the timer expired during this call.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` is unknown or the requested deadline
    /// is not representable in the timer's clock domain (state is preserved).
    ///
    /// # Panics
    ///
    /// Panics if the timer disappears between internal validation and
    /// re-lookup; both operate on `&mut self` with no interior removal in
    /// between, so the invariant is structural.
    pub fn set_posix_timer(
        &mut self,
        timer_id: i32,
        absolute: bool,
        interval: TimeSpan,
        value: TimeSpan,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> KResult<((TimeSpan, TimeSpan), Option<TimerDelivery>)> {
        let owner_pid = self.owner_pid;
        let old = self
            .posix_timers
            .get_mut(&timer_id)
            .ok_or(KError::InvalidInput)?
            .settime(absolute, interval, value, process_utime, process_stime)?;

        let signal_seq = self.allocate_posix_signal_seq();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The timer ID was validated in this \
                      exclusively borrowed manager and sequence allocation does not remove timers"
        )]
        let timer = self
            .posix_timers
            .get_mut(&timer_id)
            .expect("POSIX timer was validated before allocating its signal sequence");
        timer.set_signal_seq(signal_seq);
        timer.arm_deadline(owner_pid);
        // The timer may have already expired; update() advances the deadline
        // if so, requiring a second arm to register the new deadline.
        let expirations = timer.update(process_utime, process_stime);
        let delivery = timer.collect_delivery(timer_id, expirations);
        timer.arm_deadline(owner_pid);
        Ok((old, delivery))
    }

    /// Deletes a POSIX timer, dropping any queued alarm entry at the next
    /// poll.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` does not name a live timer.
    pub fn delete_posix_timer(&mut self, timer_id: i32) -> KResult<()> {
        self.posix_timers
            .remove(&timer_id)
            .map(|_| ())
            .ok_or(KError::InvalidInput)
    }

    /// Returns the overrun count of the most recently *delivered* POSIX
    /// timer notification, clamped to `i32::MAX`.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` does not name a live timer.
    pub fn get_posix_timer_overrun(&self, timer_id: i32) -> KResult<i32> {
        self.posix_timers
            .get(&timer_id)
            .map(PosixTimer::overrun)
            .ok_or(KError::InvalidInput)
    }

    /// Returns whether dequeuing `signal_seq` for `timer_id` consumes the
    /// owner's sampled CPU totals.
    ///
    /// The dequeue path uses this read-only check to skip the process-wide
    /// CPU-time sampling upfront: the sample walks the thread table, and it is
    /// unused for a stale generation or for a timer whose clock ignores those
    /// totals. The check applies the same admission rule
    /// [`Self::on_timer_signal_dequeued`] applies before it mutates anything.
    pub fn posix_timer_dequeue_needs_cpu_times(&self, timer_id: i32, signal_seq: u32) -> bool {
        self.posix_timers
            .get(&timer_id)
            .is_some_and(|timer| timer.signal_seq() == signal_seq && timer.is_process_cpu())
    }

    /// Test observation of a POSIX timer's registered runtime alarm deadline.
    #[cfg(unittest)]
    pub(crate) fn posix_timer_alarm_deadline(
        &self,
        timer_id: i32,
    ) -> Option<ktime_types::MonotonicInstant> {
        self.posix_timers
            .get(&timer_id)
            .and_then(PosixTimer::alarm_deadline)
    }

    /// Test observation of a POSIX timer's current signal generation.
    #[cfg(unittest)]
    pub(crate) fn posix_timer_signal_seq(&self, timer_id: i32) -> Option<u32> {
        self.posix_timers.get(&timer_id).map(PosixTimer::signal_seq)
    }

    /// Removes all POSIX timers of the owner, used on exec (`execve` keeps
    /// itimers but not POSIX timers).
    pub fn clear_posix_timers(&mut self) {
        self.posix_timers.clear();
    }

    /// Dequeue validation for POSIX timer signals.
    ///
    /// Called from the signal-dequeue observer with the `timer_id` and
    /// `signal_seq` embedded in the pending signal. A matching sequence
    /// catches the timer up to the delivery instant — an interval that elapsed
    /// since the last collection still belongs to this notification's overrun —
    /// then clears the pending-signal state (freezing `last_overrun`); a stale
    /// sequence reports `false` so the signal is dropped.
    ///
    /// `process_utime`/`process_stime` must be the owner process's current
    /// CPU-time totals; wall-clock-consuming timers read their clock
    /// internally.
    ///
    /// # Returns
    ///
    /// `true` when the notification belongs to the current timer generation
    /// and should be delivered.
    pub fn on_timer_signal_dequeued(
        &mut self,
        timer_id: i32,
        signal_seq: u32,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> bool {
        let owner_pid = self.owner_pid;
        if let Some(timer) = self.posix_timers.get_mut(&timer_id) {
            if timer.signal_seq() != signal_seq {
                return false;
            }
            if timer.catch_up_queued_overrun(process_utime, process_stime) {
                // The catch-up advanced the interval schedule, so the alarm
                // entry must follow the new deadline.
                timer.arm_deadline(owner_pid);
            }
            timer.on_signal_dequeued();
            return true;
        }
        false
    }

    fn timer_clock_now(
        timer_type: ITimerType,
        process_utime: TimeSpan,
        process_stime: TimeSpan,
    ) -> TimerInstant {
        match timer_type {
            ITimerType::Real => TimerInstant::Monotonic(monotonic_time()),
            ITimerType::Virtual => {
                TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(process_utime))
            }
            ITimerType::Prof => {
                TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(
                    process_utime.saturating_add(process_stime),
                ))
            }
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
        let deadline = timer.deadline().map(|deadline| match deadline {
            TimerInstant::Monotonic(deadline) => deadline,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: ITIMER_REAL timers are created on \
                          the monotonic clock, so their deadlines are monotonic instants"
            )]
            _ => panic!("ITIMER_REAL deadline must use the monotonic clock"),
        });
        timer.set_alarm(deadline.map(TimerInstant::Monotonic), Some(owner_pid));
    }
}

unittest::path_mod_test!(tests, "../ut-tests/manager.rs");
