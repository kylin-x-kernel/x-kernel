// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount and umount syscall entry points.

use alloc::{string::String, vec, vec::Vec};
use core::ffi::{c_char, c_void};

use fs_context::FsStruct;
use kerrno::{KError, KResult};
use kprocess::{Process, current_user_process};
use kvfs::{
    Filename, LookupFlags, LookupIntent, MntNamespace, MountFlags, Path, SuperBlockFlags,
    path::PATH_MAX,
};
use osvm::VirtPtr;
use posix_types::UserConstPtr;

const LEGACY_MOUNT_DATA_SIZE: usize = memaddr::PAGE_SIZE_4K;

/// Mount a filesystem at the specified target path.
pub fn sys_mount(
    source: UserConstPtr<c_char>,
    target: UserConstPtr<c_char>,
    fs_type: UserConstPtr<c_char>,
    flags: u32,
    data: UserConstPtr<c_void>,
) -> KResult<isize> {
    let fs_type = copy_mount_string(fs_type)?;
    let source = copy_mount_string(source)?;
    let data = copy_mount_data(data)?;
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
        data.as_deref(),
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

fn copy_mount_data(pointer: UserConstPtr<c_void>) -> KResult<Option<Vec<u8>>> {
    if pointer.is_null() {
        return Ok(None);
    }

    let bytes = pointer.cast::<u8>();
    let mut data = match bytes.load_vm_vec(LEGACY_MOUNT_DATA_SIZE) {
        Ok(data) => data,
        Err(_) => copy_mount_data_page(|offset| {
            UserConstPtr::from(bytes.as_ptr().wrapping_add(offset)).read_vm()
        })?,
    };
    data[LEGACY_MOUNT_DATA_SIZE - 1] = 0;
    Ok(Some(data))
}

fn copy_mount_data_page<E>(
    mut read_byte: impl FnMut(usize) -> Result<u8, E>,
) -> Result<Vec<u8>, E> {
    let mut data = vec![0; LEGACY_MOUNT_DATA_SIZE];
    for (offset, byte) in data.iter_mut().enumerate() {
        match read_byte(offset) {
            Ok(value) => *byte = value,
            Err(error) if offset == 0 => return Err(error),
            Err(_) => break,
        }
    }
    data[LEGACY_MOUNT_DATA_SIZE - 1] = 0;
    Ok(data)
}

fn do_mount(
    process: &Process,
    cred: &kcred::Cred,
    dev_name: Option<&str>,
    dir_name: &str,
    fs_type: Option<&str>,
    flags: u32,
    data: Option<&[u8]>,
) -> KResult<()> {
    let target = lookup_mount_path(process, dir_name, cred)?;
    path_mount(process, cred, dev_name, &target, fs_type, flags, data)
}

fn path_mount(
    process: &Process,
    cred: &kcred::Cred,
    dev_name: Option<&str>,
    target: &Path,
    fs_type: Option<&str>,
    mut flags: u32,
    data: Option<&[u8]>,
) -> KResult<()> {
    if flags & linux_raw_sys::general::MS_MGC_MSK == linux_raw_sys::general::MS_MGC_VAL {
        flags &= !linux_raw_sys::general::MS_MGC_MSK;
    }
    if flags & linux_raw_sys::general::MS_NOUSER != 0 {
        return Err(KError::InvalidInput);
    }

    // Linux authorizes the call in `path_mount()` right after the `MS_NOUSER`
    // rejection and before dispatching any operation, so an unauthorized caller
    // cannot tell which operation it asked for.
    require_mount_privilege(cred)?;

    // Reject unsupported mount operations before selecting an implemented
    // operation. Otherwise a supported bit could make an unsupported
    // combination execute with part of the request silently ignored.
    let unsupported_operations = linux_raw_sys::general::MS_MOVE
        | linux_raw_sys::general::MS_SHARED
        | linux_raw_sys::general::MS_UNBINDABLE;
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
        let super_block = target.mount().super_block();
        let source = dev_name.or_else(|| target.mount().devname());
        let mut context = kvfs::FsContext::new_reconfigure(
            super_block.as_ref(),
            source,
            data,
            superblock_flags,
            SuperBlockFlags::RDONLY,
            cred,
        )?;
        namespace.remount(target, &mut context, mount_flags)?;
        return Ok(());
    }

    let recursive = flags & linux_raw_sys::general::MS_REC != 0;
    if flags & linux_raw_sys::general::MS_BIND != 0 {
        // Linux `do_loopback()` clones the source mount flags; ordinary flags
        // in an initial bind request are ignored, including the propagation
        // bits. `MS_REMOUNT | MS_BIND` is the operation that changes the
        // cloned mount's per-mount flags.
        return do_loopback(process, cred, &namespace, target, dev_name, recursive);
    }

    // KVFS has no shared propagation groups, so `MS_PRIVATE` and `MS_SLAVE`
    // are behaviorally identical; each is validated and recorded per-mount.
    // Descendants are also covered by construction, so an accompanying
    // `MS_REC` requires no additional traversal. Linux dispatches these after
    // REMOUNT/BIND, so combinations like `MS_BIND | MS_PRIVATE` perform the
    // bind and ignore the propagation bit.
    if flags & linux_raw_sys::general::MS_PRIVATE != 0 {
        validate_propagation_flags(flags, linux_raw_sys::general::MS_PRIVATE)?;
        namespace.make_mount_private(target)?;
        return Ok(());
    }

    if flags & linux_raw_sys::general::MS_SLAVE != 0 {
        validate_propagation_flags(flags, linux_raw_sys::general::MS_SLAVE)?;
        namespace.make_mount_slave(target)?;
        return Ok(());
    }

    if recursive {
        return Err(KError::InvalidInput);
    }

    let fs_type = fs_type.ok_or(KError::InvalidInput)?;
    let file_system_type = kvfs::get_filesystem_type(fs_type).ok_or(KError::NoSuchDevice)?;
    let (root, pwd) = process.fs_context()?.lock().root_and_pwd();
    let mut context =
        kvfs::FsContext::new(file_system_type, dev_name, data, superblock_flags, cred)?;
    do_new_mount(&namespace, target, mount_flags, &mut context, &root, &pwd)
}

fn lookup_mount_path(process: &Process, name: &str, cred: &kcred::Cred) -> KResult<Path> {
    let fs = process.fs_context()?.lock().snapshot();
    lookup_mount_path_in(&fs, name, cred)
}

/// Resolves `name` against an already-snapshotted filesystem context.
///
/// `pivot_root` resolves both paths and records the old root from one snapshot,
/// so a concurrent `chroot(2)` cannot make them disagree.
fn lookup_mount_path_in(fs: &FsStruct, name: &str, cred: &kcred::Cred) -> KResult<Path> {
    Filename::new(name).lookup_at(
        fs.root(),
        fs.pwd(),
        LookupIntent::Open,
        LookupFlags::follow(),
        cred,
    )
}

/// Rejects mount namespace mutations from callers without mount privilege.
///
/// x-kernel models the mount privilege as effective UID 0, standing in for
/// Linux `CAP_SYS_ADMIN` in the mount namespace's user namespace; the check
/// reports `EPERM` like Linux `may_mount()`.
fn require_mount_privilege(cred: &kcred::Cred) -> KResult<()> {
    if cred.is_privileged() {
        Ok(())
    } else {
        Err(KError::OperationNotPermitted)
    }
}

fn do_loopback(
    process: &Process,
    cred: &kcred::Cred,
    namespace: &MntNamespace,
    mountpoint: &Path,
    old_name: Option<&str>,
    recursive: bool,
) -> KResult<()> {
    let old_name = old_name
        .filter(|name| !name.is_empty())
        .ok_or(KError::InvalidInput)?;
    let old_path = lookup_mount_path(process, old_name, cred)?;
    // Linux `do_loopback()` clones the whole source subtree for a recursive
    // bind. KVFS clones a single mount, which only matches Linux's result while
    // the source subtree has no mounts attached below it; reject the request
    // otherwise instead of silently flattening the cloned tree. The namespace
    // query looks at mounts at or below the bind source, not at every child of
    // the containing mount.
    if recursive && namespace.has_mount_under(&old_path) {
        return Err(KError::InvalidInput);
    }
    namespace.attach_bind(&old_path, mountpoint)?;
    Ok(())
}

fn do_new_mount(
    namespace: &MntNamespace,
    mountpoint: &Path,
    mount_flags: MountFlags,
    context: &mut kvfs::FsContext<'_>,
    lookup_root: &Path,
    lookup_pwd: &Path,
) -> KResult<()> {
    namespace.mount_new(mountpoint, mount_flags, context, lookup_root, lookup_pwd)?;
    Ok(())
}

fn superblock_flags(flags: u32) -> SuperBlockFlags {
    let mut superblock_flags = SuperBlockFlags::empty();
    if flags & linux_raw_sys::general::MS_RDONLY != 0 {
        superblock_flags.insert(SuperBlockFlags::RDONLY);
    }
    superblock_flags
}

fn validate_propagation_flags(flags: u32, operation: u32) -> KResult<()> {
    let allowed = operation | linux_raw_sys::general::MS_REC | linux_raw_sys::general::MS_SILENT;
    if flags & !allowed != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
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
    // Reject flags we don't implement yet: MNT_FORCE(1), MNT_EXPIRE(4),
    // UMOUNT_NOFOLLOW(8). MNT_DETACH is supported: it detaches the mount tree
    // without busy checks (lazy unmount), as required by OCI runtime rootfs
    // setups after `pivot_root`.
    let f = flags as u32;
    if f & (linux_raw_sys::general::MNT_FORCE
        | linux_raw_sys::general::MNT_EXPIRE
        | linux_raw_sys::general::UMOUNT_NOFOLLOW)
        != 0
    {
        return Err(KError::InvalidInput);
    }
    let detach = f & linux_raw_sys::general::MNT_DETACH != 0;

    let target = target.load_string()?;
    debug!("sys_umount2 <= target: {target:?}, flags: {flags} (detach: {detach})");

    let process = current_user_process();
    let cred = kprocess::current_cred();
    let target = lookup_mount_path(&process, &target, &cred)?;
    // `umount` acts on the mounted path, so resolve to the topmost covering
    // mount: after `pivot_root(".", ".")`, `umount(".", MNT_DETACH)` must
    // detach the relocated old root, not the new root underneath it.
    let target = target.resolve_final_mount();
    // Lookup errors keep their priority, but no namespace state may change
    // before authorization; Linux `can_umount()` checks the same privilege
    // before validating the mount point.
    require_mount_privilege(&cred)?;
    if detach {
        process.mnt_ns()?.detach_tree(&target)?;
    } else {
        process.mnt_ns()?.detach(&target)?;
    }
    Ok(0)
}

/// Change the mount namespace's root (`pivot_root(2)`).
///
/// Makes `new_root` (which must be a mount point) the new root of the calling
/// process's mount namespace and relocates the previous root mount at
/// `put_old` (an unmounted directory, typically inside the new root's
/// filesystem). After a successful pivot the caller's filesystem root and
/// current directory are retargeted to the new root so later `chdir("/")`
/// style lookups resolve through it, mirroring Linux `chroot_fs_refs`.
pub fn sys_pivot_root(
    new_root: UserConstPtr<c_char>,
    put_old: UserConstPtr<c_char>,
) -> KResult<isize> {
    let new_root = new_root.load_string_with_max_len(PATH_MAX)?;
    let put_old = put_old.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_pivot_root <= new_root: {new_root:?}, put_old: {put_old:?}");

    let process = current_user_process();
    let cred = kprocess::current_cred();
    // `pivot_root` rearranges the shared mount namespace root, which requires
    // mount-management privilege; path reachability alone is not authorization
    // (Linux `may_mount()`). Check before any tree surgery.
    require_mount_privilege(&cred)?;
    // Snapshot the caller's fs before resolving the paths: the lookup bases and
    // the old root used by the `chroot_fs_refs` sweep must come from the same
    // generation, or a concurrent `chroot(2)` would make them disagree.
    let fs = process.fs_context()?.lock().snapshot();
    let (old_root, _old_pwd) = fs.root_and_pwd();
    let new_root = lookup_mount_path_in(&fs, &new_root, &cred)?;
    let put_old = lookup_mount_path_in(&fs, &put_old, &cred)?;

    let namespace = process.mnt_ns()?;
    namespace.pivot_root(&new_root, &put_old, &old_root)?;

    // Linux `chroot_fs_refs(&old_root, &new_root)`: every process whose root or
    // cwd is exactly the old root moves to the new root, and anything else is
    // left alone, so a cwd outside the old root is preserved instead of being
    // reset. Processes in other mount namespaces can never match `old_root`, so
    // no namespace check is needed. When `put_old` coincided with `new_root`
    // (runc's `pivot_root(".", ".")`), the relocated old root covers this path
    // until it is umounted, so the stored path must stay unresolved to the
    // topmost mount.
    for proc in kprocess::scheduler::processes() {
        let Ok(fs_context) = proc.fs_context() else {
            continue;
        };
        let mut fs = fs_context.lock();
        if fs.root_and_pwd().0.ptr_eq(&old_root) {
            fs.set_root(new_root.clone()).unwrap_or_else(|err| {
                warn!("pivot_root: failed to move process root: {err:?}");
            });
        }
        if fs.root_and_pwd().1.ptr_eq(&old_root) {
            fs.set_pwd(new_root.clone()).unwrap_or_else(|err| {
                warn!("pivot_root: failed to move process cwd: {err:?}");
            });
        }
    }
    Ok(0)
}

unittest::path_mod_test!(tests, "../ut-tests/mount.rs");
