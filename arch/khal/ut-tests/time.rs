// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{NANOS_PER_SEC, TimeSpan};
use unittest::def_test;

#[def_test]
fn test_duration_from_nanos() {
    let nanos = NANOS_PER_SEC;
    let from = TimeSpan::from_nanos(nanos);
    let one = TimeSpan::from_secs(1);
    assert_eq!(from, one);
}

#[def_test]
fn test_duration_ordering() {
    let short = TimeSpan::from_millis(1);
    let long = TimeSpan::from_millis(2);
    assert!(long > short);
}
