// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux mount and umount compatibility entry points.

use core::ffi::{c_char, c_void};

use fs_ng_vfs::{ST_NOATIME, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_RELATIME};
use kerrno::{KError, KResult};
use kservices::vfs::MemoryFs;
use kthread::current_process_state;
use posix_types::UserConstPtr;

fn mount_flags_from_sys_mount(flags: i32) -> u32 {
    let flags = flags as u32;
    let mut mount_flags = ST_RELATIME;
    if flags & linux_raw_sys::general::MS_RDONLY != 0 {
        mount_flags |= ST_RDONLY;
    }
    if flags & linux_raw_sys::general::MS_NOSUID != 0 {
        mount_flags |= ST_NOSUID;
    }
    if flags & linux_raw_sys::general::MS_NODEV != 0 {
        mount_flags |= ST_NODEV;
    }
    if flags & linux_raw_sys::general::MS_NOEXEC != 0 {
        mount_flags |= ST_NOEXEC;
    }
    if flags & linux_raw_sys::general::MS_NOATIME != 0 {
        mount_flags |= ST_NOATIME;
    }
    if flags & linux_raw_sys::general::MS_RELATIME == 0
        && flags & linux_raw_sys::general::MS_NOATIME != 0
    {
        mount_flags &= !ST_RELATIME;
    }
    mount_flags
}

/// Mount a filesystem at the specified target path.
pub fn sys_mount(
    source: UserConstPtr<c_char>,
    target: UserConstPtr<c_char>,
    fs_type: UserConstPtr<c_char>,
    flags: i32,
    _data: UserConstPtr<c_void>,
) -> KResult<isize> {
    let source = source.load_string()?;
    let target = target.load_string()?;
    let fs_type = fs_type.load_string()?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    if fs_type != "tmpfs" {
        return Err(KError::NoSuchDevice);
    }

    let fs = MemoryFs::new_with_flags(mount_flags_from_sys_mount(flags));
    let target = current_process_state()
        .fs_context()
        .lock()
        .resolve(target)?;
    target.mount(&fs)?;

    Ok(0)
}

/// Unmount a filesystem at the specified target path.
pub fn sys_umount2(target: UserConstPtr<c_char>, _flags: i32) -> KResult<isize> {
    let target = target.load_string()?;
    debug!("sys_umount2 <= target: {target:?}");

    let target = current_process_state()
        .fs_context()
        .lock()
        .resolve(target)?;
    target.unmount()?;
    Ok(0)
}
