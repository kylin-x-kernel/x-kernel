// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock domains, local clock events and scoped software timer contracts.

#![no_std]

use kerrno::KResult;
use ktime_types::{Frequency, MonotonicInstant, TimeSpan};

/// Raw counter ticks in the platform monotonic timer domain.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct TimerTicks(u64);

impl TimerTicks {
    /// Wraps a raw hardware counter value at a timer backend boundary.
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

/// Provides a monotonic counter and conversions for its tick domain.
#[kiface::interface]
pub trait ClockSourceIf {
    /// Returns the current monotonic timer tick count.
    fn now_ticks() -> TimerTicks;
    /// Converts monotonic timer ticks to elapsed time.
    fn ticks_to_span(ticks: TimerTicks) -> TimeSpan;
    /// Returns the monotonic counter frequency.
    fn frequency() -> Frequency;
    /// Converts elapsed time to monotonic timer ticks.
    fn span_to_ticks(span: TimeSpan) -> TimerTicks;
}

/// Provides per-CPU clock events for monotonic deadlines.
#[kiface::interface]
pub trait ClockEventIf {
    /// Returns the clock-event interrupt ID.
    fn interrupt_id() -> usize;
    /// Arms the local clock event to trigger at the given deadline.
    fn arm_timer(deadline: MonotonicInstant);
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
