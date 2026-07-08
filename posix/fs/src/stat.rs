// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux stat and access compatibility entry points.

use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kfs::File;
use kprocess::current_user_process_fs_context;
use kvfs::{
    Location, LookupFlags, LookupIntent, MountFlags, NodePermission, ST_NOATIME, ST_NODEV,
    ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_NOSYMFOLLOW, ST_RDONLY, ST_RELATIME, ST_VALID,
    lookup_location,
};
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EMPTY_PATH, R_OK, W_OK, X_OK, stat, statfs, statx,
};
use posix_types::{UserConstPtr, UserPtr};

use crate::path::resolve_at;

/// Get the file metadata by `path` and write into `statbuf`.
#[cfg(target_arch = "x86_64")]
pub fn sys_stat(path: UserConstPtr<c_char>, statbuf: UserPtr<stat>) -> KResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_fstatat(AT_FDCWD, path, statbuf, 0)
}

/// Get file metadata by `fd` and write into `statbuf`.
pub fn sys_fstat(fd: i32, statbuf: UserPtr<stat>) -> KResult<isize> {
    sys_fstatat(fd, UserConstPtr::default(), statbuf, AT_EMPTY_PATH)
}

/// Get the metadata of the symbolic link and write into `buf`.
#[cfg(target_arch = "x86_64")]
pub fn sys_lstat(path: UserConstPtr<c_char>, statbuf: UserPtr<stat>) -> KResult<isize> {
    use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

    sys_fstatat(AT_FDCWD, path, statbuf, AT_SYMLINK_NOFOLLOW)
}

/// Gets file metadata relative to a directory file descriptor.
pub fn sys_fstatat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    statbuf: UserPtr<stat>,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;

    debug!("sys_fstatat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    let loc = resolve_at(dirfd, path.as_deref(), flags)?;
    statbuf.write_vm(loc.stat()?.into())?;

    Ok(0)
}

/// Gets extended file metadata (statx).
pub fn sys_statx(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    flags: u32,
    _mask: u32,
    statxbuf: UserPtr<statx>,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    debug!("sys_statx <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");

    statxbuf.write_vm(resolve_at(dirfd, path.as_deref(), flags)?.stat()?.into())?;

    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_access(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    use linux_raw_sys::general::AT_FDCWD;

    sys_faccessat2(AT_FDCWD, path, mode, 0)
}

/// Checks file accessibility with additional flags.
pub fn sys_faccessat2(
    dirfd: c_int,
    path: UserConstPtr<c_char>,
    mode: u32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    debug!("sys_faccessat2 <= dirfd: {dirfd}, path: {path:?}, mode: {mode}, flags: {flags}");

    let file = resolve_at(dirfd, path.as_deref(), flags)?;

    if mode == 0 {
        return Ok(0);
    }
    let mut required_mode = NodePermission::empty();
    if mode & R_OK != 0 {
        required_mode |= NodePermission::OWNER_READ;
    }
    if mode & W_OK != 0 {
        required_mode |= NodePermission::OWNER_WRITE;
    }
    if mode & X_OK != 0 {
        required_mode |= NodePermission::OWNER_EXEC;
    }
    let required_mode = required_mode.bits();
    if (file.stat()?.mode as u16 & required_mode) != required_mode {
        return Err(KError::PermissionDenied);
    }

    Ok(0)
}

fn superblock_statfs_flags(st_flags: u32) -> u32 {
    let mut flags = 0;
    if st_flags & ST_RDONLY != 0 {
        flags |= ST_RDONLY;
    }
    flags
}

fn statfs_flags(mnt_flags: MountFlags, st_flags: u32) -> u32 {
    let mut flags = ST_VALID | superblock_statfs_flags(st_flags);
    if mnt_flags.contains(MountFlags::RDONLY) {
        flags |= ST_RDONLY;
    }
    if mnt_flags.contains(MountFlags::NOSUID) {
        flags |= ST_NOSUID;
    }
    if mnt_flags.contains(MountFlags::NODEV) {
        flags |= ST_NODEV;
    }
    if mnt_flags.contains(MountFlags::NOEXEC) {
        flags |= ST_NOEXEC;
    }
    if mnt_flags.contains(MountFlags::NOATIME) {
        flags |= ST_NOATIME;
    }
    if mnt_flags.contains(MountFlags::NODIRATIME) {
        flags |= ST_NODIRATIME;
    }
    if mnt_flags.contains(MountFlags::RELATIME) {
        flags |= ST_RELATIME;
    }
    if mnt_flags.contains(MountFlags::NOSYMFOLLOW) {
        flags |= ST_NOSYMFOLLOW;
    }
    flags
}

fn statfs(loc: &Location) -> KResult<statfs> {
    let stat = loc.super_block().stat()?;
    // SAFETY: `statfs` is a plain Linux ABI data structure. Zeroing it
    // initializes padding and fields that this compatibility layer does not
    // currently set explicitly before copying the value to user memory.
    let mut result: statfs = unsafe { core::mem::zeroed() };
    result.f_type = stat.fs_type as _;
    result.f_bsize = stat.block_size as _;
    result.f_blocks = stat.blocks as _;
    result.f_bfree = stat.blocks_free as _;
    result.f_bavail = stat.blocks_available as _;
    result.f_files = stat.file_count as _;
    result.f_ffree = stat.free_file_count as _;
    result.f_fsid = __kernel_fsid_t {
        val: [0, loc.mountpoint().device() as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = statfs_flags(loc.mountpoint().flags(), stat.mount_flags) as _;
    Ok(result)
}

/// Gets filesystem statistics by path.
pub fn sys_statfs(path: UserConstPtr<c_char>, buf: UserPtr<statfs>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_statfs <= path: {path:?}");

    let fs_context = current_user_process_fs_context();
    let fs = fs_context.lock();
    let location = lookup_location(
        &fs.lookup_context(),
        path.as_str(),
        LookupIntent::Stat,
        LookupFlags::follow(),
    )?;
    buf.write_vm(statfs(&location.mountpoint().root_location())?)?;
    Ok(0)
}

/// Gets filesystem statistics by file descriptor.
pub fn sys_fstatfs(fd: i32, buf: UserPtr<statfs>) -> KResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    let file = kprocess::current_resources().get_file_like_as::<File>(fd)?;
    buf.write_vm(statfs(file.location())?)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use kvfs::{
        MountFlags, ST_NOATIME, ST_NODEV, ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_NOSYMFOLLOW,
        ST_RDONLY, ST_RELATIME, ST_VALID,
    };
    use unittest::{assert, assert_eq, def_test};

    #[def_test]
    fn test_statfs_flags_include_per_mount_flags() {
        let flags = MountFlags::RDONLY
            | MountFlags::NOSUID
            | MountFlags::NODEV
            | MountFlags::NOEXEC
            | MountFlags::NOATIME
            | MountFlags::NODIRATIME
            | MountFlags::RELATIME
            | MountFlags::NOSYMFOLLOW;
        let result = super::statfs_flags(flags, 0);

        assert!(result & ST_RDONLY != 0);
        assert!(result & ST_NOSUID != 0);
        assert!(result & ST_NODEV != 0);
        assert!(result & ST_NOEXEC != 0);
        assert!(result & ST_NOATIME != 0);
        assert!(result & ST_NODIRATIME != 0);
        assert!(result & ST_RELATIME != 0);
        assert!(result & ST_NOSYMFOLLOW != 0);
        assert!(result & ST_VALID != 0);
    }

    #[def_test]
    fn test_statfs_flags_preserve_supported_superblock_flags() {
        assert_eq!(
            super::statfs_flags(MountFlags::NODEV, ST_RDONLY),
            ST_VALID | ST_RDONLY | ST_NODEV
        );
    }

    #[def_test]
    fn test_superblock_statfs_flags_filter_mount_only_flags() {
        assert_eq!(
            super::superblock_statfs_flags(ST_RDONLY | ST_NODEV | ST_NOEXEC | ST_RELATIME),
            ST_RDONLY
        );
    }
}
