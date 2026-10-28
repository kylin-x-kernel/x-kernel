// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock domains, local clock events and scoped software timer contracts.

#![no_std]

use kerrno::KResult;
use ktime_types::{Frequency, MonotonicInstant, TimeSpan};

/// Tick value in the counter or logical monotonic domain named by its API.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimerTicks(u64);

impl TimerTicks {
    /// Wraps a tick representation at a counter or HAL boundary.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Exposes the raw counter representation at a hardware or ABI boundary.
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Returns the wrapping tick delta between two counter samples.
    pub const fn wrapping_duration_since(self, earlier: Self) -> Self {
        Self(self.0.wrapping_sub(earlier.0))
    }
}

/// Supplies platform counter ticks before the HAL suspend-time mapping.
///
/// Initialization and frequency must remain stable across suspend/resume.
/// Counter resets are permitted at the explicit resume boundary; while running,
/// the backend must supply a continuous counter (including any platform repair).
#[kiface::interface]
pub trait CounterSourceIf {
    /// Samples the platform counter domain, not the suspend-adjusted clock.
    fn now_ticks() -> TimerTicks;
    /// Re-establishes backend counter state at an explicit resume boundary.
    /// Must not dispatch repair IPIs or enable clock events; returns the new
    /// counter-domain anchor. The PM owner has quiesced other readers.
    fn resume_ticks() -> TimerTicks;
    /// Converts a tick duration using the initialized counter frequency.
    fn ticks_to_span(ticks: TimerTicks) -> TimeSpan;
    /// Returns the initialized counter frequency.
    fn frequency() -> Frequency;
    /// Converts a duration into counter ticks.
    fn span_to_ticks(span: TimeSpan) -> TimerTicks;
}

/// Supplies the logical monotonic clock, excluding explicit suspend intervals.
///
/// Reads must not interrupt a HAL mapping update from NMI context. PM quiesces
/// such readers before suspend/resume; ordinary IRQ reads remain supported.
/// The HAL provides this clock over CounterSourceIf. All consumers, including
/// callers outside khal, observe the same suspend-adjusted domain.
#[kiface::interface]
pub trait ClockSourceIf {
    /// Reads logical monotonic ticks; holds the suspend boundary while frozen.
    fn now_ticks() -> TimerTicks;
    /// Converts logical ticks to elapsed time.
    fn ticks_to_span(ticks: TimerTicks) -> TimeSpan;
    /// Returns the initialized counter frequency.
    fn frequency() -> Frequency;
    /// Converts elapsed time to logical ticks.
    fn span_to_ticks(span: TimeSpan) -> TimerTicks;
}

/// Provides per-CPU hardware countdowns, independent of logical clock origins.
#[kiface::interface]
pub trait ClockEventIf {
    /// Returns the clock-event interrupt ID.
    fn interrupt_id() -> usize;
    /// Arms after a non-negative interval; zero requests prompt delivery.
    /// The HAL converts logical deadlines before calling this backend.
    fn arm_timer_after(interval: TimeSpan);
    /// Restores local event configuration after controller access is restored.
    ///
    /// Leaves the countdown stopped. Runs on the initialized owning CPU with
    /// local IRQs masked and competing timer programming/repair quiesced. The
    /// platform restores the interrupt controller, vector/routing and device
    /// access first. Must not reset the clock source, calibrate, allocate,
    /// sleep, deliver callbacks or arm an event. Software deadline arbitration
    /// happens separately after timekeeping and sleep accounting are restored.
    fn restore_local_timer();
    /// Disarms the local clock event so it will not interrupt until re-armed.
    fn disarm_timer();
}

/// Borrowed one-shot timer; consumers own deadlines and the meaning of expiry.
///
/// The host keeps its allocation alive for the whole adapter body. Rearming
/// reuses that allocation and never invokes the expiry callback inline.
pub trait OneShotTimer {
    /// Starts or replaces a deadline in monotonic nanoseconds since boot.
    ///
    /// # Errors
    /// Reports invalid deadlines or CPU routing without changing the old timer.
    ///
    /// # Safety
    /// The current CPU's scheduler and timer IRQ service must be initialized.
    /// The supplied expiry callback must not sleep, unwind, enable IRQs, reenter
    /// timer dispatch or synchronously cancel this timer.
    unsafe fn arm(&mut self, deadline_ns: u64) -> KResult<()>;

    /// Removes pending expiry and waits for any running callback to return.
    ///
    /// Hold no lock needed by the callback and do not prevent its CPU's progress.
    /// Never call from the expiry callback itself.
    fn cancel_sync(&mut self);
}

/// Supplies timer mechanics, not progress intervals or quiescent observations.
#[kiface::interface]
pub trait ScopedOneShotTimerIf {
    /// Lends an initially inactive timer to exactly one synchronous body.
    ///
    /// Requires initialized per-CPU storage. Preparation follows the kernel's
    /// allocation policy; no extra allocation is needed solely for type erasure.
    /// The body may park or migrate. Expiry invokes notify_fn from timer IRQ
    /// context, and the host drains delivery before releasing storage on return.
    /// Before returning, release callback-needed locks and leave its CPU able
    /// to progress; implicit draining has the same constraints as cancel_sync.
    /// The timer borrow cannot escape this body.
    fn with_timer(notify_fn: fn(), body_fn: &mut dyn FnMut(&mut dyn OneShotTimer));
}

/// Statically linked local hardware-timer arbitration endpoint.
#[kiface::interface]
pub trait LocalTimerReprogramIf {
    /// Re-reads local deadline sources and refreshes the hardware timer.
    ///
    /// Runs with local IRQs disabled, after per-CPU timer state is initialized,
    /// during boot drain or IPI handling. Must not allocate, sleep, invoke timer
    /// callbacks or acquire a run-queue lock. The endpoint must synchronize
    /// deadline reads through the hardware write, not program a stale snapshot.
    fn on_timer_reprogram();
}

/// Reads nanoseconds in the clock source's monotonic domain.
pub fn monotonic_time_nanos() -> u64 {
    MonotonicInstant::from_span_since_origin(ClockSourceIf::ticks_to_span(
        ClockSourceIf::now_ticks(),
    ))
    .as_nanos_u64_saturating()
}

/// Counter-domain repair after a pinned idle wait, before scheduling resumes.
#[kiface::interface]
pub trait ClockResumeIf {
    /// Allows the clock-event backend to repair timer state after idle returns.
    /// `previous_ticks` is the pre-idle logical sample and may be ignored by
    /// backends that check every read. Returns whether this call repaired state,
    /// not whether a reset occurred during idle: another read may repair first.
    fn handle_idle_return(previous_ticks: TimerTicks) -> bool;
}
