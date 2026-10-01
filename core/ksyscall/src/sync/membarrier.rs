// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `membarrier(2)` syscall.
//!
//! Only `MEMBARRIER_CMD_QUERY` is implemented: the kernel currently provides
//! no expediated membarrier mechanism, so the query result advertises no
//! commands and every other command fails with `EINVAL`, matching how Linux
//! reports unsupported commands. Callers (e.g. glibc) are expected to fall
//! back to plain synchronization primitives on failure.

use kerrno::{KError, KResult};

/// Query which membarrier commands the kernel supports.
const MEMBARRIER_CMD_QUERY: i32 = 0;

/// Bitmask of supported membarrier commands; empty because no command other
/// than the query itself is implemented.
const SUPPORTED_COMMANDS: i32 = 0;

pub fn sys_membarrier(cmd: i32, flags: u32, _cpu_id: i32) -> KResult<isize> {
    // No flag-taking membarrier command is supported here, so reject any
    // non-zero flags.
    if flags != 0 {
        return Err(KError::InvalidInput);
    }

    match cmd {
        MEMBARRIER_CMD_QUERY => Ok(SUPPORTED_COMMANDS as isize),
        _ => Err(KError::InvalidInput),
    }
}
