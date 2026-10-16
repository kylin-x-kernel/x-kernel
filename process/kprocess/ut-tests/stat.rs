// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::TaskStat;

#[def_test]
fn test_taskstat_display_default() {
    let stat = TaskStat::default();
    let text = alloc::format!("{stat}");
    let text = text.trim_end();
    assert!(text.starts_with("0 ("));
    assert!(text.ends_with(" 0"));
}

#[def_test]
fn test_taskstat_display_custom() {
    let stat = TaskStat {
        pid: 7,
        comm: "init".into(),
        state: 'R',
        priority: 10,
        nice: -10,
        ..Default::default()
    };
    let text = alloc::format!("{stat}");
    assert!(text.contains(" 10 -10 "));
}
