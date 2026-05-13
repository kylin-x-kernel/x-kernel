// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount and umount compatibility entry points.

use core::ffi::{c_char, c_void};

use kerrno::{KError, KResult};
use kservices::vfs::MemoryFs;
use kthread::current_process_state;
use kvfs::{
    MountFlags, ST_NOATIME, ST_NODEV, ST_NODIRATIME, ST_NOEXEC, ST_NOSUID, ST_NOSYMFOLLOW,
    ST_RDONLY, ST_RELATIME,
};
use posix_types::UserConstPtr;

/// Map `mount(2)` MS_* flags to filesystem-level ST_* flags (for `StatFs.mount_flags`).
///
/// These are the superblock-level flags reported by `statfs(2)`.
/// They are distinct from the per-mountpoint flags produced by [`per_mount_flags`].
///
/// Keep in sync with [`per_mount_flags`] — every MS_* flag handled here
/// must also be handled there, and vice versa.
fn mount_flags_from_sys_mount(flags: i32) -> u32 {
    // `flags` is a non-negative bitmask from mount(2); safe to reinterpret.
    let f = flags as u32;
    let mut mount_flags = 0;

    // Default to relatime unless NOATIME is set.
    if f & linux_raw_sys::general::MS_NOATIME == 0 {
        mount_flags |= ST_RELATIME;
    }

    if f & linux_raw_sys::general::MS_RDONLY != 0 {
        mount_flags |= ST_RDONLY;
    }
    if f & linux_raw_sys::general::MS_NOSUID != 0 {
        mount_flags |= ST_NOSUID;
    }
    if f & linux_raw_sys::general::MS_NODEV != 0 {
        mount_flags |= ST_NODEV;
    }
    if f & linux_raw_sys::general::MS_NOEXEC != 0 {
        mount_flags |= ST_NOEXEC;
    }
    if f & linux_raw_sys::general::MS_NOATIME != 0 {
        mount_flags |= ST_NOATIME;
    }
    if f & linux_raw_sys::general::MS_NODIRATIME != 0 {
        mount_flags |= ST_NODIRATIME;
    }
    if f & linux_raw_sys::general::MS_NOSYMFOLLOW != 0 {
        mount_flags |= ST_NOSYMFOLLOW;
    }
    // STRICTATIME takes priority — clear both RELATIME and NOATIME.
    // Must stay in sync with per_mount_flags.
    if f & linux_raw_sys::general::MS_STRICTATIME != 0 {
        mount_flags &= !(ST_RELATIME | ST_NOATIME);
    }
    mount_flags
}

/// Map `mount(2)` MS_* flags to per-mount [`MountFlags`].
///
/// This returns mount-point attributes, distinct from the superblock-level
/// flags produced by [`mount_flags_from_sys_mount`].
///
/// Keep in sync with [`mount_flags_from_sys_mount`].
fn per_mount_flags(flags: i32) -> MountFlags {
    // `flags` is a non-negative bitmask from mount(2); safe to reinterpret.
    let f = flags as u32;
    let mut mnt_flags = MountFlags::empty();

    // Default to relatime unless NOATIME is set.
    if f & linux_raw_sys::general::MS_NOATIME == 0 {
        mnt_flags |= MountFlags::RELATIME;
    }

    if f & linux_raw_sys::general::MS_RDONLY != 0 {
        mnt_flags |= MountFlags::RDONLY;
    }
    if f & linux_raw_sys::general::MS_NOSUID != 0 {
        mnt_flags |= MountFlags::NOSUID;
    }
    if f & linux_raw_sys::general::MS_NODEV != 0 {
        mnt_flags |= MountFlags::NODEV;
    }
    if f & linux_raw_sys::general::MS_NOEXEC != 0 {
        mnt_flags |= MountFlags::NOEXEC;
    }
    if f & linux_raw_sys::general::MS_NOATIME != 0 {
        mnt_flags |= MountFlags::NOATIME;
    }
    if f & linux_raw_sys::general::MS_NODIRATIME != 0 {
        mnt_flags |= MountFlags::NODIRATIME;
    }
    // No explicit MS_RELATIME → MNT_RELATIME mapping: RELATIME is controlled
    // solely by the default logic above.
    // STRICTATIME takes priority — clear both RELATIME and NOATIME.
    if f & linux_raw_sys::general::MS_STRICTATIME != 0 {
        mnt_flags &= !(MountFlags::RELATIME | MountFlags::NOATIME);
    }
    if f & linux_raw_sys::general::MS_NOSYMFOLLOW != 0 {
        mnt_flags |= MountFlags::NOSYMFOLLOW;
    }
    mnt_flags
}

/// Mount a filesystem at the specified target path.
pub fn sys_mount(
    source: UserConstPtr<c_char>,
    target: UserConstPtr<c_char>,
    fs_type: UserConstPtr<c_char>,
    flags: i32,
    _data: UserConstPtr<c_void>,
) -> KResult<isize> {
    let f = flags as u32;

    // MS_NOUSER is never allowed from userspace.
    if f & linux_raw_sys::general::MS_NOUSER != 0 {
        return Err(KError::InvalidInput);
    }

    // Reject operation types that aren't yet implemented.
    // Each of these dispatches to a separate handler.
    if f & linux_raw_sys::general::MS_REMOUNT != 0 {
        return Err(KError::InvalidInput);
    }
    if f & linux_raw_sys::general::MS_BIND != 0 {
        return Err(KError::InvalidInput);
    }
    if f & linux_raw_sys::general::MS_MOVE != 0 {
        return Err(KError::InvalidInput);
    }
    if f & (linux_raw_sys::general::MS_SHARED
        | linux_raw_sys::general::MS_PRIVATE
        | linux_raw_sys::general::MS_SLAVE
        | linux_raw_sys::general::MS_UNBINDABLE
        | linux_raw_sys::general::MS_REC)
        != 0
    {
        return Err(KError::InvalidInput);
    }

    let source = source.load_string()?;
    let target = target.load_string()?;
    let fs_type = fs_type.load_string()?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, fs_type: {fs_type:?}");

    if fs_type != "tmpfs" {
        return Err(KError::NoSuchDevice);
    }

    let mount_flags = per_mount_flags(flags);
    let fs = MemoryFs::new_with_flags(mount_flags_from_sys_mount(flags));
    let target = current_process_state()
        .fs_context()
        .lock()
        .resolve(target)?;
    target.mount_with_flags(&fs, mount_flags)?;

    Ok(0)
}

/// Unmount a filesystem at the specified target path.
pub fn sys_umount2(target: UserConstPtr<c_char>, flags: i32) -> KResult<isize> {
    // Reject flags we don't implement yet: MNT_FORCE(1), MNT_DETACH(2),
    // MNT_EXPIRE(4), UMOUNT_NOFOLLOW(8).  Silently ignoring them would
    // violate user-visible semantics (e.g. MNT_DETACH would behave as a
    // synchronous unmount).
    let f = flags as u32;
    if f & (linux_raw_sys::general::MNT_FORCE
        | linux_raw_sys::general::MNT_DETACH
        | linux_raw_sys::general::MNT_EXPIRE
        | linux_raw_sys::general::UMOUNT_NOFOLLOW)
        != 0
    {
        return Err(KError::InvalidInput);
    }

    let target = target.load_string()?;
    debug!("sys_umount2 <= target: {target:?}");

    let target = current_process_state()
        .fs_context()
        .lock()
        .resolve(target)?;
    target.unmount()?;
    Ok(0)
}
