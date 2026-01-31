// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Time-related operations.

pub use core::time::Duration;
pub type TimeValue = Duration;

// Aliases for kplat names if needed locally or exposed
pub use kplat::timer::{
    MS_SEC, NS_MS, NS_SEC, NS_SEC as NANOS_PER_SEC, NS_US, NS_US as NANOS_PER_MICROS, US_SEC,
    arm_timer, freq, interrupt_id, now, now as monotonic_time, now_ns as monotonic_time_nanos,
    now_ns, now_ticks, ns2t, offset_ns, spin_until, spin_wait, t2ns, wall as wall_time, wall,
    wall_ns as wall_time_nanos, wall_ns,
};

/// Busy-wait for the given duration.
pub fn busy_wait(dur: Duration) {
    spin_wait(dur);
}

/// Busy-wait until the given deadline.
pub fn busy_wait_until(deadline: TimeValue) {
    spin_until(deadline);
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_time {
    use unittest::def_test;

    use super::{Duration, NANOS_PER_SEC};

    #[def_test]
    fn test_duration_from_nanos() {
        let nanos = NANOS_PER_SEC;
        let from = Duration::from_nanos(nanos);
        let one = Duration::from_secs(1);
        assert_eq!(from, one);
    }

    #[def_test]
    fn test_duration_ordering() {
        let short = Duration::from_millis(1);
        let long = Duration::from_millis(2);
        assert!(long > short);
    }
}
