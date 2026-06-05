// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential syscall adapters.
//!
//! Linux keeps the UID/GID syscall surface mostly in `kernel/sys.c` and the
//! supplementary-group surface in `kernel/groups.c`.

use alloc::vec::Vec;

use kcred::{CredentialError, Gid, Uid};
use kerrno::{KError, KResult};
use posix_types::{UserConstPtr, UserPtr};

/// Linux syscall value meaning "do not change this ID".
const NO_CHANGE_ID: u32 = u32::MAX;

const NGROUPS_MAX: usize = 65536;

fn optional_id(id: u32) -> Option<u32> {
    (id != NO_CHANGE_ID).then_some(id)
}

fn credential_error(err: CredentialError) -> KError {
    match err {
        CredentialError::PermissionDenied => KError::OperationNotPermitted,
    }
}

/// Get the real user ID of the current process.
pub fn sys_getuid() -> KResult<isize> {
    Ok(kthread::with_current_credentials(|credentials| {
        credentials.ruid() as isize
    }))
}

/// Get the effective user ID of the current process.
pub fn sys_geteuid() -> KResult<isize> {
    Ok(kthread::with_current_credentials(|credentials| {
        credentials.euid() as isize
    }))
}

/// Get the real group ID of the current process.
pub fn sys_getgid() -> KResult<isize> {
    Ok(kthread::with_current_credentials(|credentials| {
        credentials.rgid() as isize
    }))
}

/// Get the effective group ID of the current process.
pub fn sys_getegid() -> KResult<isize> {
    Ok(kthread::with_current_credentials(|credentials| {
        credentials.egid() as isize
    }))
}

/// Set the user ID of the current process.
pub fn sys_setuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setuid <= uid: {uid}");
    kthread::with_current_credentials_mut(|credentials| credentials.set_uid(uid))
        .map_err(credential_error)?;
    Ok(0)
}

/// Set the group ID of the current process.
pub fn sys_setgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setgid <= gid: {gid}");
    kthread::with_current_credentials_mut(|credentials| credentials.set_gid(gid))
        .map_err(credential_error)?;
    Ok(0)
}

/// Set the real and/or effective user ID of the current process.
pub fn sys_setreuid(ruid: u32, euid: u32) -> KResult<isize> {
    debug!("sys_setreuid <= ruid: {ruid}, euid: {euid}");
    kthread::with_current_credentials_mut(|credentials| {
        credentials.set_reuid(optional_id(ruid), optional_id(euid))
    })
    .map_err(credential_error)?;
    Ok(0)
}

/// Set the real and/or effective group ID of the current process.
pub fn sys_setregid(rgid: u32, egid: u32) -> KResult<isize> {
    debug!("sys_setregid <= rgid: {rgid}, egid: {egid}");
    kthread::with_current_credentials_mut(|credentials| {
        credentials.set_regid(optional_id(rgid), optional_id(egid))
    })
    .map_err(credential_error)?;
    Ok(0)
}

/// Set the real, effective, and saved user IDs of the current process.
pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> KResult<isize> {
    debug!("sys_setresuid <= ruid: {ruid}, euid: {euid}, suid: {suid}");
    kthread::with_current_credentials_mut(|credentials| {
        credentials.set_resuid(optional_id(ruid), optional_id(euid), optional_id(suid))
    })
    .map_err(credential_error)?;
    Ok(0)
}

/// Set the real, effective, and saved group IDs of the current process.
pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> KResult<isize> {
    debug!("sys_setresgid <= rgid: {rgid}, egid: {egid}, sgid: {sgid}");
    kthread::with_current_credentials_mut(|credentials| {
        credentials.set_resgid(optional_id(rgid), optional_id(egid), optional_id(sgid))
    })
    .map_err(credential_error)?;
    Ok(0)
}

/// Get the real, effective, and saved user IDs of the current process.
pub fn sys_getresuid(ruid: UserPtr<Uid>, euid: UserPtr<Uid>, suid: UserPtr<Uid>) -> KResult<isize> {
    let (current_ruid, current_euid, current_suid) =
        kthread::with_current_credentials(|credentials| {
            (credentials.ruid(), credentials.euid(), credentials.suid())
        });

    ruid.write_vm(current_ruid)?;
    euid.write_vm(current_euid)?;
    suid.write_vm(current_suid)?;
    Ok(0)
}

/// Get the real, effective, and saved group IDs of the current process.
pub fn sys_getresgid(rgid: UserPtr<Gid>, egid: UserPtr<Gid>, sgid: UserPtr<Gid>) -> KResult<isize> {
    let (current_rgid, current_egid, current_sgid) =
        kthread::with_current_credentials(|credentials| {
            (credentials.rgid(), credentials.egid(), credentials.sgid())
        });

    rgid.write_vm(current_rgid)?;
    egid.write_vm(current_egid)?;
    sgid.write_vm(current_sgid)?;
    Ok(0)
}

/// Set the filesystem user ID of the current process.
pub fn sys_setfsuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setfsuid <= uid: {uid}");
    let old_fsuid = kthread::with_current_credentials_mut(|credentials| {
        if uid == NO_CHANGE_ID {
            credentials.fsuid()
        } else {
            credentials.set_fsuid(uid)
        }
    });
    Ok(old_fsuid as isize)
}

/// Set the filesystem group ID of the current process.
pub fn sys_setfsgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setfsgid <= gid: {gid}");
    let old_fsgid = kthread::with_current_credentials_mut(|credentials| {
        if gid == NO_CHANGE_ID {
            credentials.fsgid()
        } else {
            credentials.set_fsgid(gid)
        }
    });
    Ok(old_fsgid as isize)
}

/// Get the supplementary group IDs of the current process.
pub fn sys_getgroups(size: i32, list: UserPtr<Gid>) -> KResult<isize> {
    debug!("sys_getgroups <= size: {size}");
    if size < 0 {
        return Err(KError::InvalidInput);
    }

    let groups = kthread::with_current_credentials(|credentials| {
        credentials.supplementary_groups_snapshot()
    });
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
    if !kthread::with_current_credentials(|credentials| credentials.is_privileged()) {
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

    kthread::with_current_credentials_mut(|credentials| {
        // Recheck after copy-in in case another thread changed process credentials.
        if !credentials.is_privileged() {
            return Err(KError::OperationNotPermitted);
        }
        credentials.set_supplementary_groups(groups);
        Ok(())
    })?;
    Ok(0)
}
