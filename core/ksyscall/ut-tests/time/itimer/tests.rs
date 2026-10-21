// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test(user, serial)]
fn test_sys_alarm_reports_previous_remaining_seconds() {
    // No alarm pending: both the cancel and the first arm report 0.
    assert_eq!(sys_alarm(0).unwrap(), 0);
    assert_eq!(sys_alarm(3).unwrap(), 0);

    // Replacing the pending ~3s alarm returns the whole seconds left,
    // rounded up (Linux `alarm(2)` semantics).
    let prev = sys_alarm(0).unwrap();
    assert!(
        (2..=3).contains(&prev),
        "previous remaining seconds should round to 2 or 3, got {prev}"
    );
}
