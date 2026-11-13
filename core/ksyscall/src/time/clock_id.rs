// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signed clock-ID parsing at the syscall boundary.

use kfd_objects::timerfd::TimerFdClock;
use ktime_types::ClockId;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE,
    CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_COARSE,
    CLOCK_THREAD_CPUTIME_ID,
};

/// Parse supported Linux clock IDs without unsigned wrapping.
/// Negative FD-encoded clocks and unknown positive IDs remain unsupported.
pub(super) fn parse_clock_id(raw: __kernel_clockid_t) -> Option<ClockId> {
    match raw {
        value if value == CLOCK_REALTIME as __kernel_clockid_t => Some(ClockId::Realtime),
        value if value == CLOCK_MONOTONIC as __kernel_clockid_t => Some(ClockId::Monotonic),
        value if value == CLOCK_PROCESS_CPUTIME_ID as __kernel_clockid_t => {
            Some(ClockId::ProcessCpu)
        }
        value if value == CLOCK_THREAD_CPUTIME_ID as __kernel_clockid_t => Some(ClockId::ThreadCpu),
        value if value == CLOCK_MONOTONIC_RAW as __kernel_clockid_t => Some(ClockId::MonotonicRaw),
        value if value == CLOCK_REALTIME_COARSE as __kernel_clockid_t => {
            Some(ClockId::RealtimeCoarse)
        }
        value if value == CLOCK_MONOTONIC_COARSE as __kernel_clockid_t => {
            Some(ClockId::MonotonicCoarse)
        }
        value if value == CLOCK_BOOTTIME as __kernel_clockid_t => Some(ClockId::Boottime),
        _ => None,
    }
}

/// Decodes the target PID of a Linux process scheduler clock.
fn process_sched_clock_target(raw: i32) -> Option<u32> {
    const CPUCLOCK_SCHED: i32 = 2;
    const CPUCLOCK_KIND_MASK: i32 = 0b111;
    if raw >= 0 || raw & CPUCLOCK_KIND_MASK != CPUCLOCK_SCHED {
        return None;
    }
    u32::try_from(!(raw >> 3)).ok()
}

/// Resolves the supported process scheduler clock only when it names this
/// process: zero means current, while a nonzero PID is namespace-relative.
pub(super) fn is_current_process_sched_clock(raw: i32, process_pid: u32) -> bool {
    process_sched_clock_target(raw).is_some_and(|pid| pid == 0 || pid == process_pid)
}

/// Resolves the clocks supported by clock_gettime/getres for the caller.
pub(super) fn parse_query_clock_id(raw: __kernel_clockid_t, is_gettime: bool) -> Option<ClockId> {
    if let Some(clock) = parse_clock_id(raw) {
        return Some(clock);
    }
    let target_pid = process_sched_clock_target(raw)?;
    if target_pid == 0 {
        return Some(ClockId::ProcessCpu);
    }
    let thread = kprocess::current_user_thread();
    (target_pid == thread.process().visible_pid()
        || (is_gettime
            && thread
                .task_number()
                .nr_in(&thread.process().active_pid_namespace())
                == Some(target_pid)))
    .then_some(ClockId::ProcessCpu)
}

/// Restrict a semantic clock to the set supported by timerfd.
pub(super) fn timerfd_clock(clock: ClockId) -> Option<TimerFdClock> {
    match clock {
        ClockId::Realtime => Some(TimerFdClock::Realtime),
        ClockId::Monotonic => Some(TimerFdClock::Monotonic),
        ClockId::Boottime => Some(TimerFdClock::Boottime),
        ClockId::ProcessCpu
        | ClockId::ThreadCpu
        | ClockId::MonotonicRaw
        | ClockId::RealtimeCoarse
        | ClockId::MonotonicCoarse => None,
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/time_clock_id.rs");
