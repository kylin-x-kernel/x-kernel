// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::{MAX_TIMER_INTERVAL_TICKS, deadline_to_interval_ticks};

#[def_test]
fn expired_deadline_uses_zero_countdown() {
    assert_eq!(deadline_to_interval_ticks(0, 0, 1), 0);
    assert_eq!(deadline_to_interval_ticks(99, 100, 4_000_000_000), 0);
    assert_eq!(deadline_to_interval_ticks(100, 100, 1_000_000_000), 0);
}

#[def_test]
fn countdown_rounds_positive_subtick_up() {
    assert_eq!(deadline_to_interval_ticks(101, 100, 1), 1);
    assert_eq!(deadline_to_interval_ticks(101, 100, 62_500_000), 1);
    assert_eq!(deadline_to_interval_ticks(116, 100, 62_500_000), 1);
    assert_eq!(deadline_to_interval_ticks(117, 100, 62_500_000), 2);
    assert_eq!(deadline_to_interval_ticks(101, 100, 4_000_000_000), 4);
}

#[def_test]
fn far_deadline_clamps_before_conversion_can_wrap() {
    // The old absolute conversion at 4 GHz truncated 2^64 ticks to zero.
    assert_eq!(
        deadline_to_interval_ticks(1 << 62, 1, 4_000_000_000),
        MAX_TIMER_INTERVAL_TICKS
    );
    assert_eq!(
        deadline_to_interval_ticks(u64::MAX, 0, u32::MAX as u64),
        MAX_TIMER_INTERVAL_TICKS
    );
    assert_eq!(
        deadline_to_interval_ticks(u64::MAX, u64::MAX - 1, 4_000_000_000),
        4
    );
    assert_eq!(
        deadline_to_interval_ticks(MAX_TIMER_INTERVAL_TICKS, 0, 1_000_000_000),
        MAX_TIMER_INTERVAL_TICKS
    );
}

#[def_test]
fn bounded_countdown_matches_wide_integer_reference() {
    for frequency in [
        1,
        32_768,
        19_200_000,
        62_500_000,
        1_000_000_000,
        u32::MAX as u64,
    ] {
        for now in [0, 1, 1_000_000, 1 << 62, u64::MAX - 1] {
            for deadline in [0, 1, 17, 1_000_000_001, 1 << 62, u64::MAX - 1, u64::MAX] {
                let remaining = u128::from(deadline.saturating_sub(now));
                let expected = (remaining * u128::from(frequency))
                    .div_ceil(u128::from(ktime_types::NANOS_PER_SEC))
                    .min(u128::from(MAX_TIMER_INTERVAL_TICKS))
                    as u64;
                assert_eq!(
                    deadline_to_interval_ticks(deadline, now, frequency),
                    expected
                );
            }
        }
    }
}
