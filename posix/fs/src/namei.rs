// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path-link and namespace mutation syscalls.

use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kvfs::{Filename, LookupIntent, RenameFlags};
use linux_raw_sys::general::*;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::{resolve_at_with_cred, with_fs_at};

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

    let supported_flags = AT_SYMLINK_FOLLOW | AT_EMPTY_PATH;
    if flags & !supported_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();

    let source_lookup_flags = if flags & AT_SYMLINK_FOLLOW != 0 {
        flags & !AT_SYMLINK_FOLLOW
    } else {
        flags | AT_SYMLINK_NOFOLLOW
    };
    let old = resolve_at_with_cred(old_dirfd, old_path.as_deref(), source_lookup_flags, &cred)?
        .into_path()?;
    let new_filename = Filename::new(new_path.as_str());
    with_fs_at(new_dirfd, &new_filename, |fs| {
        new_filename
            .link_at(fs.root(), fs.pwd(), &old, &cred)
            .map(|_| 0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_link(old_path: UserConstPtr<c_char>, new_path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// Removes a directory entry.
pub fn sys_unlinkat(dirfd: i32, path: UserConstPtr<c_char>, flags: usize) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");
    if flags & !(AT_REMOVEDIR as usize) != 0 {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        if flags == AT_REMOVEDIR as usize {
            filename.rmdir_at(fs.root(), fs.pwd(), &cred)?;
        } else {
            filename.unlink_at(fs.root(), fs.pwd(), &cred)?;
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
    let cred = kprocess::current_cred();
    let filename = Filename::new(linkpath.as_str());

    with_fs_at(new_dirfd, &filename, |fs| {
        filename
            .symlink_at(fs.root(), fs.pwd(), &target, &cred)
            .map(|_| 0)
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
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        let link = filename.readlink_at(fs.root(), fs.pwd(), &cred)?;
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
    let cred = kprocess::current_cred();
    let old_filename = Filename::new(old_path.as_str());
    let new_filename = Filename::new(new_path.as_str());

    let (old_dir, old_name) = with_fs_at(old_dirfd, &old_filename, |fs| {
        old_filename
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open, &cred)
            .and_then(|lookup| lookup.into_normal())
    })?;
    let (new_dir, new_name) = with_fs_at(new_dirfd, &new_filename, |fs| {
        new_filename
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open, &cred)
            .and_then(|lookup| lookup.into_normal())
    })?;

    old_dir.rename(&old_name, &new_dir, &new_name, flags, &cred)?;
    Ok(0)
}
