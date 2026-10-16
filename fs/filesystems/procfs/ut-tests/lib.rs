// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn test_new_procfs_has_expected_name_and_flags() {
    let fs = new_procfs(SuperBlockFlags::empty());

    assert_eq!(fs.name(), "proc");
    let stat = fs.stat().unwrap();
    assert_eq!(stat.fs_type, 0x9fa0);
}
