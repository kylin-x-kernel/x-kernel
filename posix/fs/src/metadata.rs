// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem metadata update syscalls.

use core::{
    ffi::{c_char, c_long},
    time::Duration,
};

use kerrno::{KError, KResult};
use khal::time::wall_time;
use kvfs::{MetadataUpdate, NodePermission};
use linux_raw_sys::general::*;
#[cfg(target_arch = "x86_64")]
use posix_types::utimbuf;
use posix_types::{TimeValueLike, UserConstPtr};

use crate::path::resolve_at;

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
    let loc = resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?;
    let meta = loc.metadata()?;

    let mut mode = meta.mode;
    mode.remove(NodePermission::SET_UID);
    if mode.contains(NodePermission::GROUP_EXEC) {
        mode.remove(NodePermission::SET_GID);
    }

    let uid = if uid == -1 { meta.uid } else { uid as _ };
    let gid = if gid == -1 { meta.gid } else { gid as _ };
    loc.update_metadata(MetadataUpdate {
        owner: Some((uid, gid)),
        mode: Some(mode),
        ..Default::default()
    })?;
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
    resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?
        .update_metadata(MetadataUpdate {
            mode: Some(NodePermission::from_bits_truncate(mode as u16)),
            ..Default::default()
        })?;
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    flags: u32,
) -> KResult<()> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    resolve_at(dirfd, path.as_deref(), flags)?
        .into_file()
        .ok_or(KError::BadFileDescriptor)?
        .update_metadata(MetadataUpdate {
            atime,
            mtime,
            ..Default::default()
        })?;
    Ok(())
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utime(path: UserConstPtr<c_char>, times: UserConstPtr<utimbuf>) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let times = times.read_vm()?;
        (
            Duration::from_secs(times.actime as _),
            Duration::from_secs(times.modtime as _),
        )
    } else {
        let time = wall_time();
        (time, time)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utimes(path: UserConstPtr<c_char>, times: UserConstPtr<[timeval; 2]>) -> KResult<isize> {
    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        (atime.try_into_time_value()?, mtime.try_into_time_value()?)
    } else {
        let time = wall_time();
        (time, time)
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

    fn utime_to_duration(time: &timespec) -> Option<KResult<Duration>> {
        match time.tv_nsec {
            val if val == UTIME_OMIT as c_long => None,
            val if val == UTIME_NOW as c_long => Some(Ok(wall_time())),
            _ => Some(time.try_into_time_value()),
        }
    }

    let (atime, mtime) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        (
            utime_to_duration(&atime).transpose()?,
            utime_to_duration(&mtime).transpose()?,
        )
    } else {
        let time = wall_time();
        (Some(time), Some(time))
    };
    if atime.is_none() && mtime.is_none() {
        return Ok(0);
    }

    update_times(dirfd, path, atime, mtime, flags)?;
    Ok(0)
}
