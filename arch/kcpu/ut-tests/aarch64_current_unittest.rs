// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn entry_task_matches_kernel_current() {
    let irq_state = karch::save_irq_and_disable();
    let current = current_task_ptr();
    let entry_task = ENTRY_TASK.current_shared().load(Ordering::Acquire);
    karch::restore_irq(irq_state);
    assert!(!current.is_null());
    assert_eq!(entry_task, current as usize);
}
