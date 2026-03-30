// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem mounting syscalls.
//!
//! This module implements filesystem mounting and unmounting operations including:
//! - Mount filesystem (mount, etc.)
//! - Unmount filesystem (umount, umount2, etc.)
//! - Mount operations and flags

use core::ffi::{c_char, c_void};

use fs_ng_vfs::{ST_NOATIME, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_RELATIME};
use kerrno::{KError, KResult};
use kfs::FS_CONTEXT;
use kserveices::mm::vm_load_string;

use crate::kernel::vfs::MemoryFs;

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

/// Mount a filesystem at the specified target path
///
/// Currently only supports tmpfs (temporary memory-based filesystem).
/// The source is loaded from user memory but not validated since tmpfs doesn't use source device names.
pub fn sys_mount(
    source: *const c_char,
    target: *const c_char,
    fs_type: *const c_char,
    flags: i32,
    _data: *const c_void,
) -> KResult<isize> {
    // Load filesystem type string from user memory
    let source = vm_load_string(source)?;
    let target = vm_load_string(target)?;
    let fs_type = vm_load_string(fs_type)?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    // Only tmpfs is supported - reject unsupported filesystem types
    if fs_type != "tmpfs" {
        return Err(KError::NoSuchDevice);
    }

    // Create a new in-memory filesystem instance
    let fs = MemoryFs::new_with_flags(mount_flags_from_sys_mount(flags));

    // Resolve the target mount point path and attach the filesystem
    let target = FS_CONTEXT.lock().resolve(target)?;
    target.mount(&fs)?;

    Ok(0)
}

/// Unmount a filesystem at the specified target path
///
/// Removes the filesystem mounted at the target path and detaches it from the directory tree.
/// The mounted filesystem must be empty or the unmount may fail depending on the filesystem implementation.
pub fn sys_umount2(target: *const c_char, _flags: i32) -> KResult<isize> {
    // Load target path from user memory
    let target = vm_load_string(target)?;
    debug!("sys_umount2 <= target: {target:?}");

    // Resolve the mount point path and detach the filesystem
    let target = FS_CONTEXT.lock().resolve(target)?;
    target.unmount()?;
    Ok(0)
}
