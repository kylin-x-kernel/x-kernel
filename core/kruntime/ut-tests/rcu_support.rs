// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reader access for controlled integration fixtures with retained tasks.

pub(super) fn reader(task: &ktask::TaskInner) -> &krcu_core::TaskReader {
    // SAFETY: the borrow retains task storage. Remote tasks are inspected only
    // through atomic snapshots. Scope entry is confined to retained current,
    // including strictly nested permitted handlers; enrollment and cleanup
    // additionally retain the exclusion and original-node lifetime required by
    // the core contracts. Each mutating call documents its fixture's boundary.
    unsafe { task.rcu_reader_for_integration() }
}
