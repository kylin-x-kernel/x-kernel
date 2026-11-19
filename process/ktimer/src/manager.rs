// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned timer manager.

use alloc::{collections::BTreeMap, vec::Vec};

use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use ktime_types::{ProcessCpuInstant, TaskCputime, TimeSpan};
use posix_types::ITimerType;

use crate::{
    Pid,
    delivery::{TimerDelivery, TimerSignal},
    interval_timer::{ITIMER_SIGNAL_CAPACITY, ITimer, TimerInstant, timer_signal},
    posix_timer::{PosixTimer, PosixTimerConfig, PosixTimerCreateError},
};

const ITIMER_REAL_INDEX: usize = ITimerType::Real as usize;
const ITIMER_VIRTUAL_INDEX: usize = ITimerType::Virtual as usize;
const ITIMER_PROF_INDEX: usize = ITimerType::Prof as usize;

/// Ownership removed from the live timer map, retained for unlocked destruction.
///
/// Drop only after releasing the timer-manager lock. This owner cannot rearm or
/// complete the removed timer and performs no cancellation acknowledgement.
#[must_use = "release retired timer ownership outside the timer-manager lock"]
pub struct RetiredPosixTimer {
    timer: PosixTimer,
}

/// A detached timer map, including its storage, for unlocked destruction.
///
/// Taking this owner publishes an empty live map without allocating a replacement.
/// Drop it after releasing the timer-manager lock, not inside the transaction.
#[must_use = "release retired timer ownership outside the timer-manager lock"]
pub struct RetiredPosixTimers {
    timers: BTreeMap<i32, PosixTimer>,
}

impl Drop for RetiredPosixTimer {
    fn drop(&mut self) {
        drop(self.timer.cancel_notifications());
    }
}

impl Drop for RetiredPosixTimers {
    fn drop(&mut self) {
        for timer in self.timers.values() {
            drop(timer.cancel_notifications());
        }
    }
}

/// Rearm result retaining old-generation cancellation until the caller unlocks.
#[must_use = "finish the timer update outside the timer-manager lock"]
pub struct PosixTimerUpdate {
    old: (TimeSpan, TimeSpan),
    delivery: Option<TimerDelivery>,
    cancellation: Option<ksignal::TimerSignalCancellation>,
}

impl PosixTimerUpdate {
    /// Removes invalidated pending ownership before returning the syscall result.
    /// Call outside timer and signal locks; cancellation does not complete a receipt.
    pub fn finish(self) -> ((TimeSpan, TimeSpan), Option<TimerDelivery>) {
        drop(self.cancellation);
        (self.old, self.delivery)
    }
}

/// A manager for process-shared signal-driven interval timers.
pub struct ProcessTimerManager {
    owner_pid: Pid,
    itimers: [ITimer; ITIMER_SIGNAL_CAPACITY],
    next_posix_timer_id: i32,
    next_posix_signal_seq: Option<u64>,
    posix_timers: BTreeMap<i32, PosixTimer>,
}

impl ProcessTimerManager {
    /// Creates a new [`ProcessTimerManager`].
    pub fn new(owner_pid: Pid) -> Self {
        Self {
            owner_pid,
            itimers: Default::default(),
            next_posix_timer_id: 1,
            next_posix_signal_seq: Some(1),
            posix_timers: BTreeMap::new(),
        }
    }

    /// Returns the current interval timer state.
    ///
    /// `sample` is one raw process CPU-time snapshot; like Linux
    /// `posix-cpu-timers`, CPU-timer state never consumes the adjusted split.
    pub fn get_itimer(&self, timer_type: ITimerType, sample: TaskCputime) -> (TimeSpan, TimeSpan) {
        self.itimers[timer_type as usize].snapshot(Self::timer_clock_now(timer_type, sample))
    }

    /// Sets an interval timer and returns the previous state.
    ///
    /// `sample` is one raw process CPU-time snapshot; see [`Self::get_itimer`].
    pub fn set_itimer(
        &mut self,
        timer_type: ITimerType,
        interval: TimeSpan,
        remaining: TimeSpan,
        sample: TaskCputime,
    ) -> (TimeSpan, TimeSpan) {
        let now = Self::timer_clock_now(timer_type, sample);
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
            let expirations = timer.update(TimeSpan::ZERO);
            // A consumed queue entry is only a hint. If the wall clock moved
            // back before update, renew its scheduling without duplicating a
            // still-queued ticket. The ticket also deduplicates repeated polls.
            timer.arm_deadline(owner_pid);
            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }
        deliveries
    }

    /// Polls the CPU-based interval timers against one raw process CPU-time
    /// sample.
    ///
    /// Every timer in this pass shares the sample's single time base. Like
    /// Linux `posix-cpu-timers`, deadlines derive from the raw tick-classified
    /// split and the scheduler runtime; the adjusted view is reserved for
    /// `times`/`getrusage`/wait-style queries and never drives expiry.
    ///
    /// The iterated sets must stay in sync with `any_cpu_timer_armed`; see
    /// the sync note there.
    pub fn poll_cpu_timers(&mut self, sample: TaskCputime) -> Vec<TimerDelivery> {
        let process_utime = TimeSpan::from_nanos(sample.utime);
        let process_stime = TimeSpan::from_nanos(sample.stime);
        let process_sched_runtime = TimeSpan::from_nanos(sample.sum_exec_runtime);
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

            let expirations = timer.update(process_sched_runtime);
            if let Some(delivery) = timer.collect_delivery(*timer_id, expirations) {
                deliveries.push(delivery);
            }
        }

        deliveries
    }

    /// Returns whether any CPU-time-driven timer is currently armed.
    ///
    /// Used as the fast-path gate for periodic CPU-timer work: false means
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

    /// Publishes a POSIX timer from an already validated configuration.
    ///
    /// Prepare the configuration before acquiring the owner mutex. ID and
    /// sequence allocation happen here; the timer is initially disarmed.
    /// BTreeMap storage still follows the existing infallible allocation policy.
    ///
    /// # Returns
    ///
    /// The kernel timer ID allocated for the new timer.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` (`InvalidInput`) with the unconsumed configuration
    /// when the ID or internal generation space is exhausted. The caller must
    /// move the rejection outside its manager lock before releasing it or
    /// recovering its parts.
    pub fn create_posix_timer(
        &mut self,
        config: PosixTimerConfig,
    ) -> Result<i32, PosixTimerCreateError> {
        self.create_posix_timer_with_id(config, Self::allocate_posix_timer_id)
    }

    fn create_posix_timer_with_id(
        &mut self,
        config: PosixTimerConfig,
        allocate_id: impl FnOnce(&mut Self) -> KResult<i32>,
    ) -> Result<i32, PosixTimerCreateError> {
        let Some(signal_seq) = self.next_posix_signal_seq else {
            return Err(PosixTimerCreateError::new(KError::InvalidInput, config));
        };
        let timer_id = match allocate_id(self) {
            Ok(timer_id) => timer_id,
            Err(error) => return Err(PosixTimerCreateError::new(error, config)),
        };
        self.next_posix_signal_seq = signal_seq.checked_add(1);
        self.posix_timers
            .insert(timer_id, config.into_timer(timer_id, signal_seq));
        Ok(timer_id)
    }

    /// Returns the current `(interval, remaining)` state of a POSIX timer.
    ///
    /// `process_sched_runtime` is the owner process's scheduler CPU clock.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` does not name a live timer.
    pub fn get_posix_timer(
        &self,
        timer_id: i32,
        process_sched_runtime: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        self.posix_timers
            .get(&timer_id)
            .map(|timer| timer.snapshot(process_sched_runtime))
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
    /// A must-use update retaining previous state, immediate delivery and old
    /// notification cancellation. Call `finish` after releasing the manager lock.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` is unknown, generations are exhausted,
    /// or the requested deadline is not representable in the timer's clock
    /// domain (state is preserved). Exhaustion also rejects disarming; deletion
    /// remains available.
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
        process_sched_runtime: TimeSpan,
    ) -> KResult<PosixTimerUpdate> {
        let signal_seq = self.next_posix_signal_seq.ok_or(KError::InvalidInput)?;
        let owner_pid = self.owner_pid;
        let old = self
            .posix_timers
            .get_mut(&timer_id)
            .ok_or(KError::InvalidInput)?
            .settime(absolute, interval, value, process_sched_runtime)?;

        self.next_posix_signal_seq = signal_seq.checked_add(1);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The timer ID was validated in this \
                      exclusively borrowed manager and sequence allocation does not remove timers"
        )]
        let timer = self
            .posix_timers
            .get_mut(&timer_id)
            .expect("POSIX timer was validated before allocating its signal sequence");
        let cancellation = timer.cancel_notifications();
        timer.set_signal_seq(signal_seq);
        timer.arm_deadline(owner_pid);
        // The timer may have already expired; update() advances the deadline
        // if so, requiring a second arm to register the new deadline.
        let expirations = timer.update(process_sched_runtime);
        let delivery = timer.collect_delivery(timer_id, expirations);
        timer.arm_deadline(owner_pid);
        Ok(PosixTimerUpdate {
            old,
            delivery,
            cancellation,
        })
    }

    /// Removes a POSIX timer and transfers its ownership for unlocked release.
    ///
    /// Lookup and dequeue validation reject the timer immediately. Existing
    /// alarm-heap entries can still cause an owner poll, but cannot find this
    /// removed timer. Drop the returned owner outside the manager lock.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `timer_id` does not name a live timer.
    pub fn delete_posix_timer(&mut self, timer_id: i32) -> KResult<RetiredPosixTimer> {
        self.posix_timers
            .remove(&timer_id)
            .map(|timer| {
                timer.invalidate_notifications();
                RetiredPosixTimer { timer }
            })
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
    pub fn posix_timer_dequeue_needs_cpu_times(&self, timer_id: i32, signal_seq: u64) -> bool {
        self.posix_timers
            .get(&timer_id)
            .is_some_and(|timer| timer.signal_seq() == signal_seq && timer.is_process_cpu())
    }

    /// Tests whether returning this exact record needs owner CPU-time sampling.
    /// A deleted/reused timer ID is not authority to progress another record.
    pub fn posix_timer_record_needs_cpu_times(
        &self,
        timer_id: i32,
        record: &ksignal::PreparedTimerRecord,
    ) -> bool {
        self.posix_timers
            .get(&timer_id)
            .is_some_and(|timer| timer.owns_record(record) && timer.is_process_cpu())
    }

    /// Retries deferred demand when the originating record becomes available.
    ///
    /// The alarm task calls this through the unlocked runtime bridge. It may
    /// produce a one-shot delivery even after the deadline has been consumed.
    /// Record identity, not a reusable timer ID alone, authorizes the retry.
    /// This returns one lease without allocating a delivery batch. Existing
    /// wall-clock alarm scheduling retains its separate allocation policy.
    pub fn progress_posix_timer_record(
        &mut self,
        timer_id: i32,
        record: &ksignal::PreparedTimerRecord,
        process_sched_runtime: TimeSpan,
    ) -> Option<TimerDelivery> {
        let timer = self.posix_timers.get_mut(&timer_id)?;
        if !timer.owns_record(record) {
            return None;
        }
        let expirations = timer.update(process_sched_runtime);
        if timer.needs_alarm_task() {
            timer.arm_deadline(self.owner_pid);
        }
        timer.collect_delivery(timer_id, expirations)
    }

    /// Captures a pending wake destination only for this exact timer reservation.
    /// Use the returned destination after releasing the timer-manager lock.
    pub fn posix_timer_record_wake_target(
        &self,
        timer_id: i32,
        record: &ksignal::PreparedTimerRecord,
    ) -> Option<(ksignal::TimerSignalTarget, ksignal::Signo)> {
        let timer = self.posix_timers.get(&timer_id)?;
        if !timer.owns_record(record) {
            return None;
        }
        timer.pending_wake_target()
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
    pub(crate) fn posix_timer_signal_seq(&self, timer_id: i32) -> Option<u64> {
        self.posix_timers.get(&timer_id).map(PosixTimer::signal_seq)
    }

    /// Removes all POSIX timers of the owner, used on exec (`execve` keeps
    /// itimers but not POSIX timers).
    /// Does not reset the generation source, including its exhausted state.
    /// Transfers the entire map without allocation. Drop the returned owner
    /// outside the timer-manager lock.
    pub fn clear_posix_timers(&mut self) -> RetiredPosixTimers {
        for timer in self.posix_timers.values() {
            timer.invalidate_notifications();
        }
        RetiredPosixTimers {
            timers: core::mem::take(&mut self.posix_timers),
        }
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
    /// `process_sched_runtime` is the owner process's scheduler CPU clock;
    /// wall-clock-consuming timers read their clock internally.
    ///
    /// # Returns
    ///
    /// `true` when the notification belongs to the current timer generation
    /// and should be delivered.
    pub fn on_timer_signal_dequeued(
        &mut self,
        timer_id: i32,
        signal_seq: u64,
        process_sched_runtime: TimeSpan,
    ) -> bool {
        let owner_pid = self.owner_pid;
        if let Some(timer) = self.posix_timers.get_mut(&timer_id) {
            if timer.signal_seq() != signal_seq {
                return false;
            }
            if timer.catch_up_queued_overrun(process_sched_runtime) {
                // The catch-up advanced the interval schedule, so the alarm
                // entry must follow the new deadline.
                timer.arm_deadline(owner_pid);
            }
            timer.on_signal_dequeued();
            return true;
        }
        false
    }

    fn timer_clock_now(timer_type: ITimerType, sample: TaskCputime) -> TimerInstant {
        match timer_type {
            ITimerType::Real => TimerInstant::Monotonic(monotonic_time()),
            ITimerType::Virtual => TimerInstant::ProcessCpu(
                ProcessCpuInstant::from_span_since_origin(TimeSpan::from_nanos(sample.utime)),
            ),
            ITimerType::Prof => {
                TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(
                    TimeSpan::from_nanos(sample.utime.saturating_add(sample.stime)),
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
