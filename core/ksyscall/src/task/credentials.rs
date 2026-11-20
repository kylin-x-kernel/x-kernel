// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Credential syscall adapters.
//!
//! Linux keeps the UID/GID syscall surface mostly in `kernel/sys.c` and the
//! supplementary-group surface in `kernel/groups.c`.

use alloc::vec::Vec;

use kcred::{Cred, Gid, Uid, is_valid_gid};
use kerrno::{KError, KResult};
use posix_types::{UserConstPtr, UserPtr};

/// Linux syscall value meaning "do not change this ID".
const NO_CHANGE_ID: u32 = u32::MAX;

const NGROUPS_MAX: usize = 65536;

fn optional_id(id: u32) -> Option<u32> {
    (id != NO_CHANGE_ID).then_some(id)
}

fn uid_to_local(cred: &Cred, uid: Uid) -> Uid {
    kvfs::Kuid::from_raw(uid).to_inside(cred.user_ns_arc())
}

fn gid_to_local(cred: &Cred, gid: Gid) -> Gid {
    kvfs::Kgid::from_raw(gid).to_inside(cred.user_ns_arc())
}

fn uid_to_global(cred: &Cred, uid: Uid) -> KResult<Uid> {
    let kuid = kvfs::Kuid::from_inside(uid, cred.user_ns_arc());
    if kuid.is_invalid() {
        return Err(KError::InvalidInput);
    }
    Ok(kuid.as_raw())
}

fn gid_to_global(cred: &Cred, gid: Gid) -> KResult<Gid> {
    let kgid = kvfs::Kgid::from_inside(gid, cred.user_ns_arc());
    if kgid.is_invalid() {
        return Err(KError::InvalidInput);
    }
    Ok(kgid.as_raw())
}

pub(super) fn update_current_cred(update: impl FnOnce(&mut Cred) -> KResult<()>) -> KResult<()> {
    let current = kprocess::current_user_thread();
    let mut new = current.prepare_creds();
    update(&mut new)?;
    current.commit_creds(new);
    Ok(())
}

/// Get the real user ID of the current process.
pub(crate) fn sys_getuid() -> KResult<isize> {
    let cred = kprocess::current_cred();
    Ok(uid_to_local(&cred, cred.ruid()) as isize)
}

/// Get the effective user ID of the current process.
pub(crate) fn sys_geteuid() -> KResult<isize> {
    let cred = kprocess::current_cred();
    Ok(uid_to_local(&cred, cred.euid()) as isize)
}

/// Get the real group ID of the current process.
pub(crate) fn sys_getgid() -> KResult<isize> {
    let cred = kprocess::current_cred();
    Ok(gid_to_local(&cred, cred.rgid()) as isize)
}

/// Get the effective group ID of the current process.
pub(crate) fn sys_getegid() -> KResult<isize> {
    let cred = kprocess::current_cred();
    Ok(gid_to_local(&cred, cred.egid()) as isize)
}

/// Set the user ID of the current process.
pub(crate) fn sys_setuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setuid <= uid: {uid}");
    update_current_cred(|cred| {
        let uid = uid_to_global(cred, uid)?;
        cred.set_uid(uid)
    })?;
    Ok(0)
}

/// Set the group ID of the current process.
pub(crate) fn sys_setgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setgid <= gid: {gid}");
    update_current_cred(|cred| {
        let gid = gid_to_global(cred, gid)?;
        cred.set_gid(gid)
    })?;
    Ok(0)
}

/// Set the real and/or effective user ID of the current process.
pub(crate) fn sys_setreuid(ruid: u32, euid: u32) -> KResult<isize> {
    debug!("sys_setreuid <= ruid: {ruid}, euid: {euid}");
    update_current_cred(|cred| {
        let ruid = optional_id(ruid)
            .map(|id| uid_to_global(cred, id))
            .transpose()?;
        let euid = optional_id(euid)
            .map(|id| uid_to_global(cred, id))
            .transpose()?;
        cred.set_reuid(ruid, euid)
    })?;
    Ok(0)
}

/// Set the real and/or effective group ID of the current process.
pub(crate) fn sys_setregid(rgid: u32, egid: u32) -> KResult<isize> {
    debug!("sys_setregid <= rgid: {rgid}, egid: {egid}");
    update_current_cred(|cred| {
        let rgid = optional_id(rgid)
            .map(|id| gid_to_global(cred, id))
            .transpose()?;
        let egid = optional_id(egid)
            .map(|id| gid_to_global(cred, id))
            .transpose()?;
        cred.set_regid(rgid, egid)
    })?;
    Ok(0)
}

/// Set the real, effective, and saved user IDs of the current process.
pub(crate) fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> KResult<isize> {
    debug!("sys_setresuid <= ruid: {ruid}, euid: {euid}, suid: {suid}");
    update_current_cred(|cred| {
        let ruid = optional_id(ruid)
            .map(|id| uid_to_global(cred, id))
            .transpose()?;
        let euid = optional_id(euid)
            .map(|id| uid_to_global(cred, id))
            .transpose()?;
        let suid = optional_id(suid)
            .map(|id| uid_to_global(cred, id))
            .transpose()?;
        cred.set_resuid(ruid, euid, suid)
    })?;
    Ok(0)
}

/// Set the real, effective, and saved group IDs of the current process.
pub(crate) fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> KResult<isize> {
    debug!("sys_setresgid <= rgid: {rgid}, egid: {egid}, sgid: {sgid}");
    update_current_cred(|cred| {
        let rgid = optional_id(rgid)
            .map(|id| gid_to_global(cred, id))
            .transpose()?;
        let egid = optional_id(egid)
            .map(|id| gid_to_global(cred, id))
            .transpose()?;
        let sgid = optional_id(sgid)
            .map(|id| gid_to_global(cred, id))
            .transpose()?;
        cred.set_resgid(rgid, egid, sgid)
    })?;
    Ok(0)
}

/// Get the real, effective, and saved user IDs of the current process.
pub(crate) fn sys_getresuid(
    ruid: UserPtr<Uid>,
    euid: UserPtr<Uid>,
    suid: UserPtr<Uid>,
) -> KResult<isize> {
    let credentials = kprocess::current_cred();
    let (current_ruid, current_euid, current_suid) =
        (credentials.ruid(), credentials.euid(), credentials.suid());

    ruid.write_vm(uid_to_local(&credentials, current_ruid))?;
    euid.write_vm(uid_to_local(&credentials, current_euid))?;
    suid.write_vm(uid_to_local(&credentials, current_suid))?;
    Ok(0)
}

/// Get the real, effective, and saved group IDs of the current process.
pub(crate) fn sys_getresgid(
    rgid: UserPtr<Gid>,
    egid: UserPtr<Gid>,
    sgid: UserPtr<Gid>,
) -> KResult<isize> {
    let credentials = kprocess::current_cred();
    let (current_rgid, current_egid, current_sgid) =
        (credentials.rgid(), credentials.egid(), credentials.sgid());

    rgid.write_vm(gid_to_local(&credentials, current_rgid))?;
    egid.write_vm(gid_to_local(&credentials, current_egid))?;
    sgid.write_vm(gid_to_local(&credentials, current_sgid))?;
    Ok(0)
}

/// Set the filesystem user ID of the current process.
pub(crate) fn sys_setfsuid(uid: Uid) -> KResult<isize> {
    debug!("sys_setfsuid <= uid: {uid}");
    let current = kprocess::current_user_thread();
    let old = current.cred();
    let old_fsuid = old.fsuid();
    if uid != NO_CHANGE_ID {
        let mut new = old.prepare();
        if let Ok(uid) = uid_to_global(&new, uid) {
            new.set_fsuid(uid);
            if new.fsuid() != old_fsuid {
                current.commit_creds(new);
            }
        }
    }
    Ok(uid_to_local(&old, old_fsuid) as isize)
}

/// Set the filesystem group ID of the current process.
pub(crate) fn sys_setfsgid(gid: Gid) -> KResult<isize> {
    debug!("sys_setfsgid <= gid: {gid}");
    let current = kprocess::current_user_thread();
    let old = current.cred();
    let old_fsgid = old.fsgid();
    if gid != NO_CHANGE_ID {
        let mut new = old.prepare();
        if let Ok(gid) = gid_to_global(&new, gid) {
            new.set_fsgid(gid);
            if new.fsgid() != old_fsgid {
                current.commit_creds(new);
            }
        }
    }
    Ok(gid_to_local(&old, old_fsgid) as isize)
}

/// Get the supplementary group IDs of the current process.
pub(crate) fn sys_getgroups(size: i32, list: UserPtr<Gid>) -> KResult<isize> {
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
        let local: Vec<Gid> = groups
            .iter()
            .map(|gid| gid_to_local(&credentials, *gid))
            .collect();
        list.write_vm_slice(&local)?;
    }
    Ok(groups.len() as isize)
}

/// Set the supplementary group IDs of the current process.
pub(crate) fn sys_setgroups(size: i32, list: UserConstPtr<Gid>) -> KResult<isize> {
    debug!("sys_setgroups <= size: {size}");
    let cred = kprocess::current_cred();
    if !cred.user_ns().has_gid_map()
        || !cred.user_ns().setgroups_allowed()
        || !cred.has_cap_in(cred.user_ns(), kcred::CAP_SETGID)
    {
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
    if groups.iter().any(|&gid| !is_valid_gid(gid)) {
        return Err(KError::InvalidInput);
    }
    let groups: Vec<Gid> = groups
        .iter()
        .map(|gid| gid_to_global(&cred, *gid))
        .collect::<KResult<_>>()?;

    update_current_cred(|cred| {
        cred.set_supplementary_groups(groups);
        Ok(())
    })?;
    Ok(0)
}
