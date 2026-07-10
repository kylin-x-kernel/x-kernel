// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path-link and namespace mutation syscalls.

use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kvfs::{Filename, LookupFlags, LookupIntent, RenameFlags};
use linux_raw_sys::general::*;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::{resolve_at, with_fs};

/// Creates a hard link to an existing file.
pub fn sys_linkat(
    old_dirfd: c_int,
    old_path: UserConstPtr<c_char>,
    new_dirfd: c_int,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_linkat <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    if flags != 0 {
        warn!("Unsupported flags: {flags}");
    }

    let old = resolve_at(old_dirfd, old_path.as_deref(), flags)?.into_path()?;
    if old.is_dir() {
        return Err(KError::OperationNotPermitted);
    }
    let (new_dir, new_name) = with_fs(new_dirfd, |fs| {
        Filename::new(new_path.as_str()).create_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::empty(),
        )
    })?;

    new_dir.link(&new_name, &old)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_link(old_path: UserConstPtr<c_char>, new_path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// Removes a directory entry.
pub fn sys_unlinkat(dirfd: i32, path: UserConstPtr<c_char>, flags: usize) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    with_fs(dirfd, |fs| {
        let entry = Filename::new(path.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::no_follow(),
        )?;
        if flags == AT_REMOVEDIR as usize {
            entry
                .parent()
                .ok_or(KError::ResourceBusy)?
                .rmdir(entry.name())?;
        } else {
            entry
                .parent()
                .ok_or(KError::IsADirectory)?
                .unlink(entry.name())?;
        }
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rmdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, AT_REMOVEDIR as usize)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_unlink(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_symlink(target: UserConstPtr<c_char>, linkpath: UserConstPtr<c_char>) -> KResult<isize> {
    sys_symlinkat(target, AT_FDCWD, linkpath)
}

/// Creates a symbolic link relative to a directory file descriptor.
pub fn sys_symlinkat(
    target: UserConstPtr<c_char>,
    new_dirfd: i32,
    linkpath: UserConstPtr<c_char>,
) -> KResult<isize> {
    let target = target.load_string()?;
    let linkpath = linkpath.load_string()?;
    debug!("sys_symlinkat <= target: {target:?}, new_dirfd: {new_dirfd}, linkpath: {linkpath:?}");

    with_fs(new_dirfd, |fs| {
        let (dir, name) = Filename::new(linkpath.as_str()).create_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::empty(),
        )?;
        dir.symlink(&name, &target)?;
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_readlink(path: UserConstPtr<c_char>, buf: UserPtr<u8>, size: usize) -> KResult<isize> {
    sys_readlinkat(AT_FDCWD, path, buf, size)
}

/// Reads the target of a symbolic link.
pub fn sys_readlinkat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    buf: UserPtr<u8>,
    size: usize,
) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");

    with_fs(dirfd, |fs| {
        let link = Filename::new(path.as_str()).readlink_at(fs.root(), fs.pwd())?;
        let read = size.min(link.len());
        buf.write_vm_slice(&link.as_bytes()[..read])?;
        Ok(read as isize)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rename(
    old_path: UserConstPtr<c_char>,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat(AT_FDCWD, old_path, AT_FDCWD, new_path)
}

pub fn sys_renameat(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat2(old_dirfd, old_path, new_dirfd, new_path, 0)
}

pub fn sys_renameat2(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path.load_string()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_renameat2 <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    let flags = RenameFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    if flags.contains(RenameFlags::WHITEOUT) || flags.has_conflicting_modes() {
        return Err(KError::InvalidInput);
    }

    let (old_dir, old_name) = with_fs(old_dirfd, |fs| {
        Filename::new(old_path.as_str())
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open)
            .and_then(|lookup| lookup.into_normal())
    })?;
    let (new_dir, new_name) = with_fs(new_dirfd, |fs| {
        Filename::new(new_path.as_str())
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open)
            .and_then(|lookup| lookup.into_normal())
    })?;

    old_dir.rename(&old_name, &new_dir, &new_name, flags)?;
    Ok(0)
}
