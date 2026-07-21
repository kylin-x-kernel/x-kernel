// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktask::KtaskRef;

use crate::lookup;

/// Returns the number of published process identities.
pub fn process_count() -> usize {
    lookup::published_process_count()
}

/// Cleans up expired task/process-group/session lookup entries.
pub fn cleanup_task_directory() {
    lookup::cleanup_directory();
}

/// Returns the tasks currently visible in the global directory.
pub fn task_snapshot() -> alloc::vec::Vec<KtaskRef> {
    lookup::task_snapshot()
}
