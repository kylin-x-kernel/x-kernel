// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount and umount compatibility entry points.

use core::ffi::{c_char, c_void};

use kerrno::{KError, KResult};
use kthread::current_process_state;
use kvfs::{MountFlags, ST_RDONLY};
use memfs::MemoryFs;
use posix_types::UserConstPtr;

fn superblock_flags_from_sys_mount(flags: i32) -> u32 {
    let f = flags as u32;
    let mut sb_flags = 0;

    if f & linux_raw_sys::general::MS_RDONLY != 0 {
        sb_flags |= ST_RDONLY;
    }
    sb_flags
}

/// Map `mount(2)` MS_* flags to per-mount [`MountFlags`].
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
    let fs = MemoryFs::new_with_name_and_flags("tmpfs", superblock_flags_from_sys_mount(flags));
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

#[cfg(unittest)]
mod tests {
    use kvfs::{MountFlags, ST_RDONLY};
    use unittest::{assert, assert_eq, def_test};

    #[def_test]
    fn test_superblock_flags_from_mount_only_options_are_filtered() {
        let flags = (linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC
            | linux_raw_sys::general::MS_NOSUID
            | linux_raw_sys::general::MS_NOATIME
            | linux_raw_sys::general::MS_NODIRATIME
            | linux_raw_sys::general::MS_NOSYMFOLLOW) as i32;

        assert_eq!(super::superblock_flags_from_sys_mount(flags), 0);
    }

    #[def_test]
    fn test_superblock_flags_preserve_readonly() {
        assert_eq!(
            super::superblock_flags_from_sys_mount(linux_raw_sys::general::MS_RDONLY as i32),
            ST_RDONLY
        );
    }

    #[def_test]
    fn test_per_mount_flags_preserve_mount_options() {
        let flags = (linux_raw_sys::general::MS_RDONLY
            | linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC) as i32;
        let result = super::per_mount_flags(flags);

        assert!(result.contains(MountFlags::RDONLY));
        assert!(result.contains(MountFlags::NODEV));
        assert!(result.contains(MountFlags::NOEXEC));
    }
}
