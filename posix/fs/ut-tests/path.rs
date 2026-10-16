// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn absolute_at_path_does_not_select_dirfd() {
    assert_eq!(at_path_dirfd(1234, &Filename::new("/absolute")), AT_FDCWD);
    assert_eq!(at_path_dirfd(1234, &Filename::new("relative")), 1234);
}
