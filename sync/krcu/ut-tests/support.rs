// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reader access for controlled integration fixtures only.

pub(crate) fn reader(task: &ktask::TaskInner) -> &krcu_core::TaskReader {
    // SAFETY: these fixtures enter scopes only on the retained current task,
    // with enrollment/cleanup under their documented exclusion. Remote tasks
    // are inspected only through atomic snapshots; this borrow retains the
    // task storage and does not grant remote scope entry or cleanup.
    unsafe { task.rcu_reader_for_integration() }
}
