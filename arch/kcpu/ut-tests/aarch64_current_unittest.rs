// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn entry_task_matches_kernel_current() {
    let (current, entry_task) = ENTRY_TASK
        .with_current_irq_disabled(|entry| (current_task_ptr(), entry.load(Ordering::Acquire)));
    assert!(!current.is_null());
    assert_eq!(entry_task, current as usize);
}
