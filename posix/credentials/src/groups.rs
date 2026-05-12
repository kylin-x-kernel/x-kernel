// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Supplementary-group syscalls, corresponding to Linux `kernel/groups.c`.

use alloc::vec::Vec;

use kcred::Gid;
use kerrno::{KError, KResult};
use posix_types::{UserConstPtr, UserPtr};

use crate::helpers::{with_credentials, with_credentials_mut};

const NGROUPS_MAX: usize = 65536;

/// Get the supplementary group IDs of the current process.
pub fn sys_getgroups(size: i32, list: UserPtr<Gid>) -> KResult<isize> {
    debug!("sys_getgroups <= size: {size}");
    if size < 0 {
        return Err(KError::InvalidInput);
    }

    let groups = with_credentials(|credentials| credentials.supplementary_groups_snapshot());
    if size == 0 {
        return Ok(groups.len() as isize);
    }

    if groups.len() > size as usize {
        return Err(KError::InvalidInput);
    }

    if !groups.is_empty() {
        list.check_non_null().ok_or(KError::BadAddress)?;
        list.write_vm_slice(groups.as_ref())?;
    }
    Ok(groups.len() as isize)
}

/// Set the supplementary group IDs of the current process.
pub fn sys_setgroups(size: i32, list: UserConstPtr<Gid>) -> KResult<isize> {
    debug!("sys_setgroups <= size: {size}");
    if !with_credentials(|credentials| credentials.is_privileged()) {
        return Err(KError::OperationNotPermitted);
    }

    if size < 0 || size as usize > NGROUPS_MAX {
        return Err(KError::InvalidInput);
    }

    let groups = if size == 0 {
        Vec::new()
    } else {
        list.check_non_null().ok_or(KError::BadAddress)?;
        list.load_vm_vec(size as usize)?
    };

    with_credentials_mut(|credentials| {
        // Recheck after copy-in in case another thread changed process credentials.
        if !credentials.is_privileged() {
            return Err(KError::OperationNotPermitted);
        }
        credentials.set_supplementary_groups(groups);
        Ok(())
    })?;
    Ok(0)
}
