// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount and umount syscall entry points.

use core::ffi::{c_char, c_void};

use kerrno::{KError, KResult};
use kprocess::current_user_process;
use kvfs::{Filename, LookupFlags, LookupIntent, MountFlags, SuperBlockFlags, path::PATH_MAX};
use memfs::shmem;
use posix_types::UserConstPtr;

fn superblock_flags_from_sys_mount(flags: i32) -> SuperBlockFlags {
    let f = flags as u32;
    let mut sb_flags = SuperBlockFlags::empty();

    if f & linux_raw_sys::general::MS_RDONLY != 0 {
        sb_flags.insert(SuperBlockFlags::RDONLY);
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

fn bind_mount_flags(source_flags: MountFlags, flags: i32) -> MountFlags {
    let f = flags as u32;
    let mut bind_flags = source_flags;

    bind_flags |= per_mount_flags(flags) & !MountFlags::RELATIME;

    if f & linux_raw_sys::general::MS_NOATIME != 0 {
        bind_flags.remove(MountFlags::RELATIME);
    }
    if f & linux_raw_sys::general::MS_RELATIME != 0 {
        bind_flags.remove(MountFlags::NOATIME);
        bind_flags.insert(MountFlags::RELATIME);
    }
    if f & linux_raw_sys::general::MS_STRICTATIME != 0 {
        bind_flags.remove(MountFlags::RELATIME | MountFlags::NOATIME);
    }

    bind_flags
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

    // Reject operation types we do not implement. MS_MOVE and the
    // shared/private/slave/unbindable (and recursive) propagation flags are
    // unsupported; bind and remount each have a dedicated path below.
    if f & (linux_raw_sys::general::MS_MOVE
        | linux_raw_sys::general::MS_SHARED
        | linux_raw_sys::general::MS_PRIVATE
        | linux_raw_sys::general::MS_SLAVE
        | linux_raw_sys::general::MS_UNBINDABLE
        | linux_raw_sys::general::MS_REC)
        != 0
    {
        return Err(KError::InvalidInput);
    }

    let source = if source.is_null() {
        None
    } else {
        Some(source.load_string_with_max_len(PATH_MAX)?)
    };
    let source_ref = source.as_deref();
    let target = target.load_string()?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, flags: {f:#x}");

    // Operation-type dispatch follows the Linux `do_mount()` order: remount is
    // checked before bind so MS_REMOUNT|MS_BIND reaches the remount path. Both
    // accept `fs_type == NULL`, so `fs_type` is loaded only for a fresh mount.

    if f & linux_raw_sys::general::MS_REMOUNT != 0 {
        let process = current_user_process();
        let cred = kprocess::current_cred();
        let fs_struct = process.fs_context()?;
        let fs = fs_struct.lock();
        let target = Filename::new(target.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )?;
        let mount_flags = per_mount_flags(flags);
        if f & linux_raw_sys::general::MS_BIND != 0 {
            process
                .mnt_ns()?
                .reconfigure_bind_mount(&target, mount_flags)?;
        } else {
            process.mnt_ns()?.reconfigure_mount(&target, mount_flags)?;
        }
        return Ok(0);
    }

    if f & linux_raw_sys::general::MS_BIND != 0 {
        // An empty/NULL source is an invalid argument (EINVAL), not EFAULT.
        let source = source
            .filter(|source| !source.is_empty())
            .ok_or(KError::InvalidInput)?;
        let process = current_user_process();
        let cred = kprocess::current_cred();
        let fs_struct = process.fs_context()?;
        let fs = fs_struct.lock();
        let source = Filename::new(source.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )?;
        let target = Filename::new(target.as_str()).lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::follow(),
            &cred,
        )?;
        let mount_flags = bind_mount_flags(source.mount().flags(), flags);
        process
            .mnt_ns()?
            .attach_bind(&source, &target, mount_flags)?;
        return Ok(0);
    }

    let fs_type = fs_type.load_string()?;
    let mount_flags = per_mount_flags(flags);
    let superblock_flags = superblock_flags_from_sys_mount(flags);
    let mount_fs = match fs_type.as_str() {
        "devfs" | "devtmpfs" => devfs::new_devfs(superblock_flags),
        "proc" => procfs::new_procfs(superblock_flags),
        "sysfs" => {
            memfs::ramfs::new_ramfs_with_name_and_superblock_flags("sysfs", superblock_flags)
        }
        "tmpfs" => shmem::new_tmpfs(superblock_flags),
        #[cfg(feature = "ebpf")]
        "bpf" => bpffs::new_bpffs(superblock_flags),
        _ => return Err(KError::NoSuchDevice),
    };
    let process = current_user_process();
    let cred = kprocess::current_cred();
    let fs_struct = process.fs_context()?;
    let fs = fs_struct.lock();
    let target = Filename::new(target.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    )?;
    process
        .mnt_ns()?
        .attach_with_flags_and_devname(&target, &mount_fs, mount_flags, source_ref)?;

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

    let process = current_user_process();
    let cred = kprocess::current_cred();
    let fs_struct = process.fs_context()?;
    let fs = fs_struct.lock();
    let target = Filename::new(target.as_str()).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        &cred,
    )?;
    process.mnt_ns()?.detach(&target)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use kvfs::{MountFlags, SuperBlockFlags};
    use unittest::{assert, assert_eq, def_test};

    #[def_test]
    fn test_superblock_flags_from_mount_only_options_are_filtered() {
        let flags = (linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC
            | linux_raw_sys::general::MS_NOSUID
            | linux_raw_sys::general::MS_NOATIME
            | linux_raw_sys::general::MS_NODIRATIME
            | linux_raw_sys::general::MS_NOSYMFOLLOW) as i32;

        assert_eq!(
            super::superblock_flags_from_sys_mount(flags),
            SuperBlockFlags::empty()
        );
    }

    #[def_test]
    fn test_superblock_flags_preserve_readonly() {
        assert_eq!(
            super::superblock_flags_from_sys_mount(linux_raw_sys::general::MS_RDONLY as i32),
            SuperBlockFlags::RDONLY
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

    #[def_test]
    fn test_bind_mount_flags_preserve_source_and_apply_requested_options() {
        let source = MountFlags::NOSUID | MountFlags::NOATIME;
        let flags = (linux_raw_sys::general::MS_BIND
            | linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC
            | linux_raw_sys::general::MS_RELATIME) as i32;

        let result = super::bind_mount_flags(source, flags);

        assert!(result.contains(MountFlags::NOSUID));
        assert!(result.contains(MountFlags::NODEV));
        assert!(result.contains(MountFlags::NOEXEC));
        assert!(result.contains(MountFlags::RELATIME));
        assert!(!result.contains(MountFlags::NOATIME));
    }
}
