// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount and umount syscall entry points.

use alloc::string::String;
use core::ffi::{c_char, c_void};

use kerrno::{KError, KResult};
use kprocess::{Process, current_user_process};
use kvfs::{
    Filename, LookupFlags, LookupIntent, MntNamespace, MountFlags, Path, SuperBlockFlags,
    path::PATH_MAX,
};
use posix_types::UserConstPtr;

/// Mount a filesystem at the specified target path.
///
/// Filesystem-specific options in `data` are not yet forwarded to filesystem
/// backends.
pub fn sys_mount(
    source: UserConstPtr<c_char>,
    target: UserConstPtr<c_char>,
    fs_type: UserConstPtr<c_char>,
    flags: u32,
    _data: UserConstPtr<c_void>,
) -> KResult<isize> {
    let fs_type = copy_mount_string(fs_type)?;
    let source = copy_mount_string(source)?;
    let target = target.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_mount <= source: {source:?}, target: {target:?}, flags: {flags:#x}");

    let process = current_user_process();
    let cred = kprocess::current_cred();
    do_mount(
        &process,
        &cred,
        source.as_deref(),
        target.as_str(),
        fs_type.as_deref(),
        flags,
    )?;
    Ok(0)
}

fn copy_mount_string(pointer: UserConstPtr<c_char>) -> KResult<Option<String>> {
    if pointer.is_null() {
        Ok(None)
    } else {
        pointer.load_string_with_max_len(PATH_MAX).map(Some)
    }
}

fn do_mount(
    process: &Process,
    cred: &kcred::Cred,
    dev_name: Option<&str>,
    dir_name: &str,
    fs_type: Option<&str>,
    flags: u32,
) -> KResult<()> {
    let target = lookup_mount_path(process, dir_name, cred)?;
    path_mount(process, cred, dev_name, &target, fs_type, flags)
}

fn path_mount(
    process: &Process,
    cred: &kcred::Cred,
    dev_name: Option<&str>,
    target: &Path,
    fs_type: Option<&str>,
    mut flags: u32,
) -> KResult<()> {
    if flags & linux_raw_sys::general::MS_MGC_MSK == linux_raw_sys::general::MS_MGC_VAL {
        flags &= !linux_raw_sys::general::MS_MGC_MSK;
    }
    if flags & linux_raw_sys::general::MS_NOUSER != 0 {
        return Err(KError::InvalidInput);
    }

    // Reject unsupported mount operations before selecting an implemented
    // operation. Otherwise a supported bit could make an unsupported
    // combination execute with part of the request silently ignored.
    let unsupported_operations = linux_raw_sys::general::MS_MOVE
        | linux_raw_sys::general::MS_SHARED
        | linux_raw_sys::general::MS_PRIVATE
        | linux_raw_sys::general::MS_SLAVE
        | linux_raw_sys::general::MS_UNBINDABLE
        | linux_raw_sys::general::MS_REC;
    if flags & unsupported_operations != 0 {
        return Err(KError::InvalidInput);
    }

    let mount_flags = per_mount_flags(flags, target.mount().flags());
    let superblock_flags = superblock_flags(flags);
    let namespace = process.mnt_ns()?;

    if flags & (linux_raw_sys::general::MS_REMOUNT | linux_raw_sys::general::MS_BIND)
        == (linux_raw_sys::general::MS_REMOUNT | linux_raw_sys::general::MS_BIND)
    {
        namespace.reconfigure_mount(target, mount_flags)?;
        return Ok(());
    }
    if flags & linux_raw_sys::general::MS_REMOUNT != 0 {
        namespace.remount(target, superblock_flags, mount_flags)?;
        return Ok(());
    }
    if flags & linux_raw_sys::general::MS_BIND != 0 {
        // Linux `do_loopback()` clones the source mount flags; ordinary flags
        // in an initial bind request are ignored. `MS_REMOUNT | MS_BIND` is the
        // operation that changes the cloned mount's per-mount flags.
        return do_loopback(process, cred, &namespace, target, dev_name);
    }

    do_new_mount(
        &namespace,
        target,
        fs_type,
        superblock_flags,
        mount_flags,
        dev_name,
    )
}

fn lookup_mount_path(process: &Process, name: &str, cred: &kcred::Cred) -> KResult<Path> {
    let fs_context = process.fs_context()?;
    let fs = fs_context.lock();
    Filename::new(name).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        cred,
    )
}

fn do_loopback(
    process: &Process,
    cred: &kcred::Cred,
    namespace: &MntNamespace,
    mountpoint: &Path,
    old_name: Option<&str>,
) -> KResult<()> {
    let old_name = old_name
        .filter(|name| !name.is_empty())
        .ok_or(KError::InvalidInput)?;
    let old_path = lookup_mount_path(process, old_name, cred)?;
    namespace.attach_bind(&old_path, mountpoint)?;
    Ok(())
}

fn do_new_mount(
    namespace: &MntNamespace,
    mountpoint: &Path,
    fs_type: Option<&str>,
    superblock_flags: SuperBlockFlags,
    mount_flags: MountFlags,
    dev_name: Option<&str>,
) -> KResult<()> {
    let fs_type = fs_type.ok_or(KError::InvalidInput)?;
    let file_system_type = kvfs::get_filesystem_type(fs_type).ok_or(KError::NoSuchDevice)?;
    let mount_fs = file_system_type.mount_nodev(superblock_flags)?;
    namespace.attach_with_flags_and_devname(mountpoint, &mount_fs, mount_flags, dev_name)?;
    Ok(())
}

fn superblock_flags(flags: u32) -> SuperBlockFlags {
    let mut superblock_flags = SuperBlockFlags::empty();
    if flags & linux_raw_sys::general::MS_RDONLY != 0 {
        superblock_flags.insert(SuperBlockFlags::RDONLY);
    }
    superblock_flags
}

/// Map `mount(2)` MS_* flags to per-mount [`MountFlags`].
fn per_mount_flags(flags: u32, current_flags: MountFlags) -> MountFlags {
    let mut mount_flags = MountFlags::empty();

    // Linux defaults to relatime unless NOATIME is set.
    if flags & linux_raw_sys::general::MS_NOATIME == 0 {
        mount_flags |= MountFlags::RELATIME;
    }
    if flags & linux_raw_sys::general::MS_RDONLY != 0 {
        mount_flags |= MountFlags::RDONLY;
    }
    if flags & linux_raw_sys::general::MS_NOSUID != 0 {
        mount_flags |= MountFlags::NOSUID;
    }
    if flags & linux_raw_sys::general::MS_NODEV != 0 {
        mount_flags |= MountFlags::NODEV;
    }
    if flags & linux_raw_sys::general::MS_NOEXEC != 0 {
        mount_flags |= MountFlags::NOEXEC;
    }
    if flags & linux_raw_sys::general::MS_NOATIME != 0 {
        mount_flags |= MountFlags::NOATIME;
    }
    if flags & linux_raw_sys::general::MS_NODIRATIME != 0 {
        mount_flags |= MountFlags::NODIRATIME;
    }
    if flags & linux_raw_sys::general::MS_STRICTATIME != 0 {
        mount_flags &= !(MountFlags::RELATIME | MountFlags::NOATIME);
    }
    if flags & linux_raw_sys::general::MS_NOSYMFOLLOW != 0 {
        mount_flags |= MountFlags::NOSYMFOLLOW;
    }

    // A remount replaces all user-settable per-mount flags. Linux preserves
    // only the current atime mask when no atime policy is requested.
    let atime_request = linux_raw_sys::general::MS_NOATIME
        | linux_raw_sys::general::MS_NODIRATIME
        | linux_raw_sys::general::MS_RELATIME
        | linux_raw_sys::general::MS_STRICTATIME;
    if flags & linux_raw_sys::general::MS_REMOUNT != 0 && flags & atime_request == 0 {
        let atime_flags = MountFlags::NOATIME | MountFlags::NODIRATIME | MountFlags::RELATIME;
        mount_flags.remove(atime_flags);
        mount_flags.insert(current_flags & atime_flags);
    }
    mount_flags
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
    let target = lookup_mount_path(&process, &target, &cred)?;
    process.mnt_ns()?.detach(&target)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use kvfs::{MountFlags, SuperBlockFlags};
    use unittest::{assert, assert_eq, def_test};

    #[def_test]
    fn test_superblock_flags_from_mount_only_options_are_filtered() {
        let flags = linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC
            | linux_raw_sys::general::MS_NOSUID
            | linux_raw_sys::general::MS_NOATIME
            | linux_raw_sys::general::MS_NODIRATIME
            | linux_raw_sys::general::MS_NOSYMFOLLOW;

        assert_eq!(super::superblock_flags(flags), SuperBlockFlags::empty());
    }

    #[def_test]
    fn test_superblock_flags_preserve_readonly() {
        assert_eq!(
            super::superblock_flags(linux_raw_sys::general::MS_RDONLY),
            SuperBlockFlags::RDONLY
        );
    }

    #[def_test]
    fn test_per_mount_flags_preserve_mount_options() {
        let flags = linux_raw_sys::general::MS_RDONLY
            | linux_raw_sys::general::MS_NODEV
            | linux_raw_sys::general::MS_NOEXEC;
        let result = super::per_mount_flags(flags, MountFlags::empty());

        assert!(result.contains(MountFlags::RDONLY));
        assert!(result.contains(MountFlags::NODEV));
        assert!(result.contains(MountFlags::NOEXEC));
    }
}
