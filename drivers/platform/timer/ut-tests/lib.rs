// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

#[def_test]
fn test_timer_deadline_nanos_clamps_large_instant() {
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);

    assert_eq!(deadline.as_nanos_u64_saturating(), u64::MAX);
}
