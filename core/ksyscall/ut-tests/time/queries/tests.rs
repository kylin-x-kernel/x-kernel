// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use posix_types::UserPtr;
use unittest::def_test;

#[def_test]
fn time_with_null_tloc_returns_current_epoch_seconds() {
    let before = ktime::realtime().unix_seconds() as isize;
    let seconds = super::sys_time(UserPtr::default()).expect("time(NULL) must succeed");
    let after = ktime::realtime().unix_seconds() as isize;

    assert!((before..=after).contains(&seconds));
}
