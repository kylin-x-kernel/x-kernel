// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_smoltcp_deadline_clamps_expired_delay() {
    let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
    let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(9_999));

    assert_eq!(deadline.saturating_duration_since(current), TimeSpan::ZERO);
}

#[def_test]
fn test_smoltcp_deadline_preserves_future_delay() {
    let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
    let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(12_500));

    assert_eq!(
        deadline.checked_duration_since(current),
        Some(TimeSpan::from_micros(2_500))
    );
}

#[def_test]
fn deadline_crossed_during_round_is_immediate_work() {
    let current = SmoltcpInstant::from_millis(10);
    let future = SmoltcpInstant::from_millis(11);
    let later = SmoltcpInstant::from_millis(12);

    assert!(!has_immediate_timer_work(current, Some(future)));
    assert!(has_immediate_timer_work(later, Some(future)));
}

#[def_test]
fn due_timer_is_immediate_work() {
    let current = SmoltcpInstant::from_millis(10);

    assert!(has_immediate_timer_work(current, Some(current)));
}

#[def_test]
fn deferred_close_deadline_can_precede_protocol_deadline() {
    let close = SmoltcpInstant::from_millis(10);
    let protocol = SmoltcpInstant::from_millis(20);

    assert!(earliest_poll_deadline(Some(protocol), Some(close)) == Some(close));
}
