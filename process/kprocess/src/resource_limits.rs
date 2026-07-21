// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KResult;

use crate::{Pid, Process, lookup};

/// Resolves the non-exited process whose resource limits are being queried or updated.
pub fn target_process(pid: Pid) -> KResult<Arc<Process>> {
    lookup::live_process(pid)
}
