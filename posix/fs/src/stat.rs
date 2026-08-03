// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stat and access syscall entry points.

use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kprocess::current_user_process;
use kvfs::{
    Filename, LookupFlags, LookupIntent, MountFlags, NodeType, Path, Permission, StatFsFlags,
    SuperBlockFlags,
};
use linux_raw_sys::general::{
    __kernel_fsid_t, AT_EACCESS, AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW, R_OK, W_OK, X_OK, stat,
    statfs, statx,
};
use posix_types::{UserConstPtr, UserPtr};

use crate::path::{resolve_at, resolve_at_with_cred};

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

    sys_faccessat(AT_FDCWD, path, mode)
}

/// Checks file accessibility relative to a directory file descriptor.
pub fn sys_faccessat(dirfd: c_int, path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_faccessat2(dirfd, path, mode, 0)
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

    if mode & !(R_OK | W_OK | X_OK) != 0
        || flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0
    {
        return Err(KError::InvalidInput);
    }

    let current = kprocess::current_cred();
    let real_id_cred;
    let access_cred = if flags & AT_EACCESS != 0 {
        current.as_ref()
    } else {
        real_id_cred = current.for_access();
        &real_id_cred
    };
    let path = resolve_at_with_cred(dirfd, path.as_deref(), flags, access_cred)?.into_path()?;

    let mut permission = Permission::MAY_ACCESS;
    if mode & R_OK != 0 {
        permission |= Permission::MAY_READ;
    }
    if mode & W_OK != 0 {
        permission |= Permission::MAY_WRITE;
    }
    if mode & X_OK != 0 {
        permission |= Permission::MAY_EXEC;
    }

    if permission.contains(Permission::MAY_EXEC)
        && path.is_regular_file()
        && path.mount().flags().contains(MountFlags::NOEXEC)
    {
        return Err(KError::PermissionDenied);
    }
    path.permission(permission, access_cred)?;

    if permission.contains(Permission::MAY_WRITE)
        && path.mount().flags().contains(MountFlags::RDONLY)
        && matches!(
            path.node_type(),
            NodeType::RegularFile | NodeType::Directory | NodeType::Symlink
        )
    {
        return Err(KError::ReadOnlyFilesystem);
    }

    Ok(0)
}

fn superblock_statfs_flags(superblock_flags: SuperBlockFlags) -> StatFsFlags {
    // Match Linux `fs/statfs.c::flags_by_sb()`: `SB_NOATIME` and
    // `SB_NODIRATIME` affect inode behavior but their `ST_*` counterparts are
    // exported only from the per-mount flags.
    let mut flags = StatFsFlags::empty();
    if superblock_flags.contains(SuperBlockFlags::RDONLY) {
        flags.insert(StatFsFlags::RDONLY);
    }
    flags
}

fn statfs_flags(mnt_flags: MountFlags, superblock_flags: SuperBlockFlags) -> StatFsFlags {
    let mut flags = StatFsFlags::VALID | superblock_statfs_flags(superblock_flags);
    if mnt_flags.contains(MountFlags::RDONLY) {
        flags.insert(StatFsFlags::RDONLY);
    }
    if mnt_flags.contains(MountFlags::NOSUID) {
        flags.insert(StatFsFlags::NOSUID);
    }
    if mnt_flags.contains(MountFlags::NODEV) {
        flags.insert(StatFsFlags::NODEV);
    }
    if mnt_flags.contains(MountFlags::NOEXEC) {
        flags.insert(StatFsFlags::NOEXEC);
    }
    if mnt_flags.contains(MountFlags::NOATIME) {
        flags.insert(StatFsFlags::NOATIME);
    }
    if mnt_flags.contains(MountFlags::NODIRATIME) {
        flags.insert(StatFsFlags::NODIRATIME);
    }
    if mnt_flags.contains(MountFlags::RELATIME) {
        flags.insert(StatFsFlags::RELATIME);
    }
    if mnt_flags.contains(MountFlags::NOSYMFOLLOW) {
        flags.insert(StatFsFlags::NOSYMFOLLOW);
    }
    flags
}

fn statfs(loc: &Path) -> KResult<statfs> {
    let stat = loc.filesystem_stat()?;
    // SAFETY: `statfs` is a plain Linux ABI data structure. Zeroing it
    // initializes padding and fields that this syscall path does not
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
        val: [0, loc.mount().synthetic_device_id() as _],
    };
    result.f_namelen = stat.name_length as _;
    result.f_frsize = stat.fragment_size as _;
    result.f_flags = statfs_flags(loc.mount().flags(), loc.mount().super_block_flags()).bits() as _;
    Ok(result)
}

/// Gets filesystem statistics by path.
pub fn sys_statfs(path: UserConstPtr<c_char>, buf: UserPtr<statfs>) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_statfs <= path: {path:?}");

    let process = current_user_process();
    let fs_struct = process.fs_context()?;
    let fs = fs_struct.lock();
    let cred = kprocess::current_cred();
    let location = Filename::new(path.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Stat,
        LookupFlags::follow(),
        &cred,
    )?;
    buf.write_vm(statfs(&location.mount().root_path())?)?;
    Ok(0)
}

/// Gets filesystem statistics by file descriptor.
pub fn sys_fstatfs(fd: i32, buf: UserPtr<statfs>) -> KResult<isize> {
    debug!("sys_fstatfs <= fd: {fd}");

    let resources = current_user_process().resources()?;
    let file = resources.get_file(fd)?;
    buf.write_vm(statfs(file.path())?)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use kvfs::{MountFlags, StatFsFlags, SuperBlockFlags};
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
        let result = super::statfs_flags(flags, SuperBlockFlags::empty());

        assert!(result.contains(StatFsFlags::RDONLY));
        assert!(result.contains(StatFsFlags::NOSUID));
        assert!(result.contains(StatFsFlags::NODEV));
        assert!(result.contains(StatFsFlags::NOEXEC));
        assert!(result.contains(StatFsFlags::NOATIME));
        assert!(result.contains(StatFsFlags::NODIRATIME));
        assert!(result.contains(StatFsFlags::RELATIME));
        assert!(result.contains(StatFsFlags::NOSYMFOLLOW));
        assert!(result.contains(StatFsFlags::VALID));
    }

    #[def_test]
    fn test_statfs_flags_preserve_supported_superblock_flags() {
        assert_eq!(
            super::statfs_flags(MountFlags::NODEV, SuperBlockFlags::RDONLY),
            StatFsFlags::VALID | StatFsFlags::RDONLY | StatFsFlags::NODEV
        );
    }

    #[def_test]
    fn test_superblock_statfs_flags_filter_mount_only_flags() {
        assert_eq!(
            super::superblock_statfs_flags(
                SuperBlockFlags::RDONLY | SuperBlockFlags::NOATIME | SuperBlockFlags::NODIRATIME
            ),
            StatFsFlags::RDONLY
        );
    }
}
