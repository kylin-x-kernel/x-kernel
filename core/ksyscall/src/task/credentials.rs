// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential syscall adapters.
//!
//! Linux keeps the UID/GID syscall surface mostly in `kernel/sys.c` and the
//! supplementary-group surface in `kernel/groups.c`.

use alloc::vec::Vec;

use kcred::{Cred, Gid, Uid};
use kerrno::{KError, KResult};
use posix_types::{UserConstPtr, UserPtr};

/// Linux syscall value meaning "do not change this ID".
const NO_CHANGE_ID: u32 = u32::MAX;

const NGROUPS_MAX: usize = 65536;

fn optional_id(id: u32) -> Option<u32> {
    (id != NO_CHANGE_ID).then_some(id)
}

pub(super) fn update_current_cred(update: impl FnOnce(&mut Cred) -> KResult<()>) -> KResult<()> {
    let current = kprocess::current_user_thread();
    let mut new = current.prepare_creds();
    update(&mut new)?;
    current.commit_creds(new);
    Ok(())
}

/// Get the real user ID of the current process.
pub fn sys_getuid() -> KResult<isize> {
    Ok(kprocess::current_cred().ruid() as isize)
}

/// Get the effective user ID of the current process.
pub fn sys_geteuid() -> KResult<isize> {
    Ok(kprocess::current_cred().euid() as isize)
}

/// Get the real group ID of the current process.
pub fn sys_getgid() -> KResult<isize> {
    Ok(kprocess::current_cred().rgid() as isize)
}

/// Get the effective group ID of the current process.
pub fn sys_getegid() -> KResult<isize> {
    Ok(kprocess::current_cred().egid() as isize)
}

/// Set the user ID of the current process.
pub fn sys_setuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setuid <= uid: {uid}");
    update_current_cred(|cred| cred.set_uid(uid))?;
    Ok(0)
}

/// Set the group ID of the current process.
pub fn sys_setgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setgid <= gid: {gid}");
    update_current_cred(|cred| cred.set_gid(gid))?;
    Ok(0)
}

/// Set the real and/or effective user ID of the current process.
pub fn sys_setreuid(ruid: u32, euid: u32) -> KResult<isize> {
    debug!("sys_setreuid <= ruid: {ruid}, euid: {euid}");
    update_current_cred(|cred| cred.set_reuid(optional_id(ruid), optional_id(euid)))?;
    Ok(0)
}

/// Set the real and/or effective group ID of the current process.
pub fn sys_setregid(rgid: u32, egid: u32) -> KResult<isize> {
    debug!("sys_setregid <= rgid: {rgid}, egid: {egid}");
    update_current_cred(|cred| cred.set_regid(optional_id(rgid), optional_id(egid)))?;
    Ok(0)
}

/// Set the real, effective, and saved user IDs of the current process.
pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> KResult<isize> {
    debug!("sys_setresuid <= ruid: {ruid}, euid: {euid}, suid: {suid}");
    update_current_cred(|cred| {
        cred.set_resuid(optional_id(ruid), optional_id(euid), optional_id(suid))
    })?;
    Ok(0)
}

/// Set the real, effective, and saved group IDs of the current process.
pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> KResult<isize> {
    debug!("sys_setresgid <= rgid: {rgid}, egid: {egid}, sgid: {sgid}");
    update_current_cred(|cred| {
        cred.set_resgid(optional_id(rgid), optional_id(egid), optional_id(sgid))
    })?;
    Ok(0)
}

/// Get the real, effective, and saved user IDs of the current process.
pub fn sys_getresuid(ruid: UserPtr<Uid>, euid: UserPtr<Uid>, suid: UserPtr<Uid>) -> KResult<isize> {
    let credentials = kprocess::current_cred();
    let (current_ruid, current_euid, current_suid) =
        (credentials.ruid(), credentials.euid(), credentials.suid());

    ruid.write_vm(current_ruid)?;
    euid.write_vm(current_euid)?;
    suid.write_vm(current_suid)?;
    Ok(0)
}

/// Get the real, effective, and saved group IDs of the current process.
pub fn sys_getresgid(rgid: UserPtr<Gid>, egid: UserPtr<Gid>, sgid: UserPtr<Gid>) -> KResult<isize> {
    let credentials = kprocess::current_cred();
    let (current_rgid, current_egid, current_sgid) =
        (credentials.rgid(), credentials.egid(), credentials.sgid());

    rgid.write_vm(current_rgid)?;
    egid.write_vm(current_egid)?;
    sgid.write_vm(current_sgid)?;
    Ok(0)
}

/// Set the filesystem user ID of the current process.
pub fn sys_setfsuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setfsuid <= uid: {uid}");
    let current = kprocess::current_user_thread();
    let old = current.cred();
    let old_fsuid = old.fsuid();
    if uid != NO_CHANGE_ID {
        let mut new = old.prepare();
        new.set_fsuid(uid);
        if new.fsuid() != old_fsuid {
            current.commit_creds(new);
        }
    }
    Ok(old_fsuid as isize)
}

/// Set the filesystem group ID of the current process.
pub fn sys_setfsgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setfsgid <= gid: {gid}");
    let current = kprocess::current_user_thread();
    let old = current.cred();
    let old_fsgid = old.fsgid();
    if gid != NO_CHANGE_ID {
        let mut new = old.prepare();
        new.set_fsgid(gid);
        if new.fsgid() != old_fsgid {
            current.commit_creds(new);
        }
    }
    Ok(old_fsgid as isize)
}

/// Get the supplementary group IDs of the current process.
pub fn sys_getgroups(size: i32, list: UserPtr<Gid>) -> KResult<isize> {
    debug!("sys_getgroups <= size: {size}");
    if size < 0 {
        return Err(KError::InvalidInput);
    }

    let credentials = kprocess::current_cred();
    let groups = credentials.supplementary_groups();
    if size == 0 {
        return Ok(groups.len() as isize);
    }

    if groups.len() > size as usize {
        return Err(KError::InvalidInput);
    }

    if !groups.is_empty() {
        list.check_non_null().ok_or(KError::BadAddress)?;
        list.write_vm_slice(groups)?;
    }
    Ok(groups.len() as isize)
}

/// Set the supplementary group IDs of the current process.
pub fn sys_setgroups(size: i32, list: UserConstPtr<Gid>) -> KResult<isize> {
    debug!("sys_setgroups <= size: {size}");
    if !kprocess::current_cred().is_privileged() {
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

    update_current_cred(|cred| {
        cred.set_supplementary_groups(groups);
        Ok(())
    })?;
    Ok(0)
}
