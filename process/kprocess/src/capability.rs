// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KResult;

use crate::{Pid, lookup};

/// Validates that a capability-target PID names a non-zombie process.
pub fn validate_target_pid(pid: Pid) -> KResult<()> {
    let _ = lookup::live_process(pid)?;
    Ok(())
}
