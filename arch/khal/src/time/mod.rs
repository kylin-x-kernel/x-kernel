// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified time capability exposed by `khal`.

pub use kiface_time_api::{ClockEventIf, ClockSourceIf, CounterSourceIf, TimerTicks};
use ktime_types::{Frequency, MonotonicInstant, TimeSpan};

mod epoch;
mod suspend;
pub use suspend::{
    MonotonicSuspendError, MonotonicSuspendToken, resume_monotonic_clock, suspend_monotonic_clock,
};

#[kiface::provide]
impl ClockSourceIf {
    fn now_ticks() -> TimerTicks {
        TimerTicks::from_raw(
            suspend::CLOCK
                .read(|| CounterSourceIf::now_ticks().as_raw())
                .0,
        )
    }

    fn ticks_to_span(ticks: TimerTicks) -> TimeSpan {
        CounterSourceIf::ticks_to_span(ticks)
    }

    fn frequency() -> Frequency {
        CounterSourceIf::frequency()
    }

    fn span_to_ticks(span: TimeSpan) -> TimerTicks {
        CounterSourceIf::span_to_ticks(span)
    }
}

#[inline]
pub fn now_ticks() -> TimerTicks {
    ClockSourceIf::now_ticks()
}

#[inline]
pub fn ticks_to_span(ticks: TimerTicks) -> TimeSpan {
    ClockSourceIf::ticks_to_span(ticks)
}

#[inline]
pub fn frequency() -> Frequency {
    ClockSourceIf::frequency()
}

#[inline]
pub fn span_to_ticks(span: TimeSpan) -> TimerTicks {
    ClockSourceIf::span_to_ticks(span)
}

#[inline]
pub fn interrupt_id() -> usize {
    ClockEventIf::interrupt_id()
}

#[inline]
pub fn arm_timer(deadline: MonotonicInstant) {
    // PM must exclude reprogramming across epoch changes. Keep the CPU pinned
    // from sampling through the local hardware write, as on the timer IRQ path.
    let _guard = kspin::NoPreemptIrqSave::new();
    let (ticks, is_suspended) = suspend::CLOCK.read(|| CounterSourceIf::now_ticks().as_raw());
    let now = MonotonicInstant::from_span_since_origin(ticks_to_span(TimerTicks::from_raw(ticks)));
    match timer_interval(deadline, now, is_suspended) {
        Some(interval) => ClockEventIf::arm_timer_after(interval),
        None => ClockEventIf::disarm_timer(),
    }
}

fn timer_interval(
    deadline: MonotonicInstant,
    now: MonotonicInstant,
    is_suspended: bool,
) -> Option<TimeSpan> {
    if is_suspended {
        None
    } else {
        Some(deadline.saturating_duration_since(now))
    }
}

#[inline]
pub fn disarm_timer() {
    ClockEventIf::disarm_timer()
}

#[inline]
pub fn handle_idle_return(previous_ticks: TimerTicks) -> bool {
    kiface_time_api::ClockResumeIf::handle_idle_return(previous_ticks)
}

#[inline]
pub fn monotonic_time() -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(ticks_to_span(now_ticks()))
}

/// Nanoseconds since the monotonic origin, for hardware / scheduler arithmetic.
#[inline]
pub fn monotonic_time_nanos() -> u64 {
    monotonic_time().as_nanos_u64_saturating()
}

#[inline]
pub fn spin_until(deadline: MonotonicInstant) {
    while monotonic_time() < deadline {
        core::hint::spin_loop();
    }
}

#[inline]
pub fn spin_wait(dur: TimeSpan) {
    spin_until(monotonic_time() + dur);
}

/// Busy-wait for the given duration.
pub fn busy_wait(dur: TimeSpan) {
    spin_wait(dur);
}

/// Busy-wait until the given deadline.
pub fn busy_wait_until(deadline: MonotonicInstant) {
    spin_until(deadline);
}

#[cfg(unittest)]
unittest::path_mod_test!(pub mod tests_time, "../../ut-tests/time.rs");
