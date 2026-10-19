// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

#[def_test]
fn current_task_ptr_is_consistent() {
    let first = crate::current_task_ptr();
    let second = crate::current_task_ptr();
    assert_eq!(first, second);
}
