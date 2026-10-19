// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn unsigned_seconds_reject_values_outside_system_time() {
    assert_eq!(
        system_time_from_unsigned_seconds(i64::MAX as u64),
        Some(SystemTime::from_unix_seconds(i64::MAX))
    );
    assert_eq!(system_time_from_unsigned_seconds(u64::MAX), None);
}
