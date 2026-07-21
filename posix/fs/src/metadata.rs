// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem metadata update syscalls.

use core::ffi::{c_char, c_long};
#[cfg(target_arch = "x86_64")]
use core::time::Duration;

use kerrno::KResult;
use khal::time::wall_time;
use kvfs::{NodePermission, SetattrTime};
use linux_raw_sys::general::*;
#[cfg(target_arch = "x86_64")]
use posix_types::utimbuf;
use posix_types::{TimeValueLike, UserConstPtr};

use crate::path::resolve_at_with_cred;

#[cfg(target_arch = "x86_64")]
pub fn sys_chown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_lchown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fchown(fd: i32, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(fd, UserConstPtr::default(), uid, gid, AT_EMPTY_PATH)
}

/// Changes file ownership relative to a directory file descriptor.
pub fn sys_fchownat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    uid: i32,
    gid: i32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let cred = kprocess::current_cred();
    let loc = resolve_at_with_cred(dirfd, path.as_deref(), flags, &cred)?.into_path()?;
    loc.chown(
        (uid != -1).then_some(uid as u32),
        (gid != -1).then_some(gid as u32),
        &cred,
    )?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chmod(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_fchmodat(AT_FDCWD, path, mode, 0)
}

/// Changes file permissions by file descriptor.
pub fn sys_fchmod(fd: i32, mode: u32) -> KResult<isize> {
    sys_fchmodat(fd, UserConstPtr::default(), mode, AT_EMPTY_PATH)
}

/// Changes file permissions relative to a directory file descriptor.
pub fn sys_fchmodat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    mode: u32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let cred = kprocess::current_cred();
    let loc = resolve_at_with_cred(dirfd, path.as_deref(), flags, &cred)?.into_path()?;
    loc.chmod(NodePermission::from_bits_truncate(mode as u16), &cred)?;
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    atime: Option<SetattrTime>,
    mtime: Option<SetattrTime>,
    flags: u32,
) -> KResult<()> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let cred = kprocess::current_cred();
    let loc = resolve_at_with_cred(dirfd, path.as_deref(), flags, &cred)?.into_path()?;
    loc.set_times(atime, mtime, &cred)?;
    Ok(())
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utime(path: UserConstPtr<c_char>, times: UserConstPtr<utimbuf>) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let times = times.read_vm()?;
        (
            SetattrTime::Explicit(Duration::from_secs(times.actime as _)),
            SetattrTime::Explicit(Duration::from_secs(times.modtime as _)),
        )
    } else {
        let time = wall_time();
        (SetattrTime::Current(time), SetattrTime::Current(time))
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utimes(path: UserConstPtr<c_char>, times: UserConstPtr<[timeval; 2]>) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        (
            SetattrTime::Explicit(atime.try_into_time_value()?),
            SetattrTime::Explicit(mtime.try_into_time_value()?),
        )
    } else {
        let time = wall_time();
        (SetattrTime::Current(time), SetattrTime::Current(time))
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

pub fn sys_utimensat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    times: UserConstPtr<[timespec; 2]>,
    mut flags: u32,
) -> KResult<isize> {
    if path.is_null() {
        flags |= AT_EMPTY_PATH;
    }

    fn utime_to_update(time: &timespec) -> Option<KResult<SetattrTime>> {
        match time.tv_nsec {
            val if val == UTIME_OMIT as c_long => None,
            val if val == UTIME_NOW as c_long => Some(Ok(SetattrTime::Current(wall_time()))),
            _ => Some(time.try_into_time_value().map(SetattrTime::Explicit)),
        }
    }

    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        (
            utime_to_update(&atime).transpose()?,
            utime_to_update(&mtime).transpose()?,
        )
    } else {
        let time = wall_time();
        (
            Some(SetattrTime::Current(time)),
            Some(SetattrTime::Current(time)),
        )
    };
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    update_times(dirfd, path, atime, mtime, flags)?;
    Ok(0)
}
