// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn statx_reports_all_populated_basic_fields() {
    let result = statx::from(Kstat::default());
    assert_eq!(result.stx_mask & STATX_BASIC_STATS, STATX_BASIC_STATS);
}
