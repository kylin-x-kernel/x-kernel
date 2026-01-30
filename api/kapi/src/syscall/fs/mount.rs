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

use kerrno::{KError, KResult};
use kfs::FS_CONTEXT;

use crate::{mm::vm_load_string, vfs::MemoryFs};

/// Mount a filesystem at the specified target path
///
/// Currently only supports tmpfs (temporary memory-based filesystem).
/// The source is loaded from user memory but not validated since tmpfs doesn't use source device names.
pub fn sys_mount(
    source: *const c_char,
    target: *const c_char,
    fs_type: *const c_char,
    _flags: i32,
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
    let fs = MemoryFs::new();

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
