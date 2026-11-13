// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! New mount API: `fsopen(2)`, `fsconfig(2)`, `fsmount(2)`, `move_mount(2)`.
//!
//! systemd mounts service credential stores and builds service mount
//! namespaces with these calls. An fs-context fd accumulates mount options;
//! `FSCONFIG_CMD_CREATE` constructs the superblock; `fsmount(2)` wraps it in a
//! detached mount; and `move_mount(2)` attaches that mount at a target path.
//! Only the subset systemd exercises is implemented; unsupported requests
//! report `EINVAL`/`EOPNOTSUPP` so callers can fall back.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::ffi::{c_char, c_int, c_void};

use anon_inodefs::AnonInodeFs;
use kerrno::{KError, KResult};
use ksync::Mutex;
use kvfs::{
    FMode, FileOperations, Filename, FsContext, LookupFlags, LookupIntent, Mount, MountFlags,
    OpenFlags, SuperBlock, SuperBlockFlags, VfsFile, VfsInode, VfsResult, dentry_open,
    path::PATH_MAX,
};
use linux_raw_sys::general::{
    FSMOUNT_CLOEXEC, FSOPEN_CLOEXEC, MOUNT_ATTR_NODEV, MOUNT_ATTR_NOEXEC, MOUNT_ATTR_NOSUID,
    MOUNT_ATTR_NOSYMFOLLOW, MOUNT_ATTR_RDONLY, MOVE_MOUNT_F_EMPTY_PATH, O_DIRECTORY, O_RDONLY,
};
use posix_types::UserConstPtr;

use crate::{mount::require_mount_privilege, path::with_fs_at};

// fsconfig(2) commands; linux_raw_sys does not expose them.
const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSCONFIG_CMD_RECONFIGURE: u32 = 7;

/// Accumulated state of one fs-context fd between `fsopen` and `fsmount`.
struct FsContextState {
    fs_type: String,
    source: Option<String>,
    /// Filesystem-specific options rendered as `key=value` / `key` tokens and
    /// joined into the legacy mount-data string at `FSCONFIG_CMD_CREATE`.
    options: Vec<String>,
    sb_flags: SuperBlockFlags,
    super_block: Option<Arc<SuperBlock>>,
}

struct FsContextFd {
    state: Mutex<FsContextState>,
}

/// A detached mount produced by `fsmount`, consumed by `move_mount`.
struct MountFd {
    mount: Mutex<Option<Arc<Mount>>>,
}

struct MountApiFops;

impl FileOperations for MountApiFops {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        Ok(())
    }
}

fn context_fd(file: &VfsFile) -> KResult<Arc<FsContextFd>> {
    file.private_data_get::<FsContextFd>()
        .ok_or(KError::BadFileDescriptor)
}

fn mount_fd(file: &VfsFile) -> KResult<Arc<MountFd>> {
    file.private_data_get::<MountFd>()
        .ok_or(KError::BadFileDescriptor)
}

/// Opens a filesystem context for `fs_type` (`fsopen(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's credentials and
/// installs the new fd into the caller's fd table. It may sleep (anonymous-inode
/// file creation and fd allocation), so it must not be called from IRQ context
/// or while holding a spinlock or IRQ-preemption guard.
pub fn sys_fsopen(fs_type: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let fs_type = fs_type.load_string_with_max_len(PATH_MAX)?;
    debug!("sys_fsopen <= fs_type: {fs_type:?}, flags: {flags:#x}");
    if flags & !FSOPEN_CLOEXEC != 0 {
        return Err(KError::InvalidInput);
    }
    // Creating a filesystem context can lead to a new superblock and mount;
    // Linux gates fsopen(2) on CAP_SYS_ADMIN like mount(2).
    require_mount_privilege(&kprocess::current_cred())?;
    kvfs::get_filesystem_type(&fs_type).ok_or(KError::NoSuchDevice)?;
    let state = FsContextState {
        fs_type,
        source: None,
        options: Vec::new(),
        sb_flags: SuperBlockFlags::empty(),
        super_block: None,
    };
    let file = AnonInodeFs::global().get_file(
        "[fscontext]",
        Arc::new(MountApiFops),
        Arc::new(FsContextFd {
            state: Mutex::new(state),
        }),
        FMode::READ | FMode::WRITE,
        OpenFlags::READ_WRITE,
        kprocess::current_cred(),
    )?;
    kprocess::current_files()
        .add_file(file, flags & FSOPEN_CLOEXEC != 0)
        .map(|fd| fd as isize)
}

/// Configures an fs-context fd (`fsconfig(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table and
/// credentials. `FSCONFIG_CMD_CREATE`/`CMD_RECONFIGURE` build or reconfigure a
/// superblock and may sleep (filesystem `get_tree`/`reconfigure` callbacks), so
/// it must not be called from IRQ context or while holding a spinlock or
/// IRQ-preemption guard.
pub fn sys_fsconfig(
    fd: c_int,
    cmd: u32,
    key: UserConstPtr<c_char>,
    value: UserConstPtr<c_void>,
    _aux: c_int,
) -> KResult<isize> {
    let file = kprocess::current_files().get_file(fd)?;
    let ctx = context_fd(&file)?;
    // fsconfig can create or reconfigure a superblock; enforce the same mount
    // authority as fsopen/mount(2) rather than relying on the fd alone.
    require_mount_privilege(&kprocess::current_cred())?;
    let mut state = ctx.state.lock();
    match cmd {
        FSCONFIG_SET_FLAG => {
            let key = key.load_string_with_max_len(PATH_MAX)?;
            // `ro` is a superblock flag, not a filesystem mount option.
            if key == "ro" {
                state.sb_flags.insert(SuperBlockFlags::RDONLY);
            } else {
                state.options.push(key);
            }
        }
        FSCONFIG_SET_STRING => {
            let key = key.load_string_with_max_len(PATH_MAX)?;
            let value = value.cast::<c_char>().load_string_with_max_len(PATH_MAX)?;
            // Linux treats the `source` key as the device/backing source, not a
            // filesystem option; block-device get_tree_bdev reads it from the
            // fs context. Folding it into the option string would both lose the
            // source and pollute the mount data.
            if key == "source" {
                state.source = Some(value);
            } else {
                state.options.push(alloc::format!("{key}={value}"));
            }
        }
        FSCONFIG_CMD_CREATE => {
            if state.super_block.is_some() {
                return Err(KError::InvalidInput);
            }
            let data = state.options.join(",");
            let fs_type = kvfs::get_filesystem_type(&state.fs_type).ok_or(KError::NoSuchDevice)?;
            let process = kprocess::current_user_process();
            let cred = kprocess::current_cred();
            let (root, pwd) = process.fs_context()?.lock().root_and_pwd();
            let mut context = FsContext::new(
                fs_type,
                state.source.as_deref(),
                Some(data.as_bytes()),
                state.sb_flags,
                &cred,
            )?;
            state.super_block = Some(context.get_tree(&root, &pwd)?);
        }
        FSCONFIG_CMD_RECONFIGURE => {
            let super_block = state.super_block.as_ref().ok_or(KError::InvalidInput)?;
            let cred = kprocess::current_cred();
            let mut context = FsContext::new_reconfigure(
                super_block.as_ref(),
                state.source.as_deref(),
                None,
                state.sb_flags,
                state.sb_flags,
                &cred,
            )?;
            super_block.reconfigure(&mut context)?;
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(0)
}

/// Maps `fsmount(2)` `MOUNT_ATTR_*` bits to per-mount [`MountFlags`].
fn mount_attr_to_flags(attr: u32) -> MountFlags {
    let mut flags = MountFlags::empty();
    if attr & MOUNT_ATTR_RDONLY != 0 {
        flags |= MountFlags::RDONLY;
    }
    if attr & MOUNT_ATTR_NOSUID != 0 {
        flags |= MountFlags::NOSUID;
    }
    if attr & MOUNT_ATTR_NODEV != 0 {
        flags |= MountFlags::NODEV;
    }
    if attr & MOUNT_ATTR_NOEXEC != 0 {
        flags |= MountFlags::NOEXEC;
    }
    if attr & MOUNT_ATTR_NOSYMFOLLOW != 0 {
        flags |= MountFlags::NOSYMFOLLOW;
    }
    flags
}

/// Creates a detached mount from a prepared fs-context fd (`fsmount(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table and mount
/// namespace. It may sleep (`dentry_open` of the detached root and fd
/// allocation), so it must not be called from IRQ context or while holding a
/// spinlock or IRQ-preemption guard.
pub fn sys_fsmount(fd: c_int, flags: u32, attr_flags: u32) -> KResult<isize> {
    debug!("sys_fsmount <= fd: {fd}, flags: {flags:#x}, attr_flags: {attr_flags:#x}");
    if flags & !FSMOUNT_CLOEXEC != 0 {
        return Err(KError::InvalidInput);
    }
    // fsmount creates a detached mount; Linux requires CAP_SYS_ADMIN.
    require_mount_privilege(&kprocess::current_cred())?;
    let file = kprocess::current_files().get_file(fd)?;
    let ctx = context_fd(&file)?;
    let (super_block, source) = {
        let state = ctx.state.lock();
        let super_block = state.super_block.clone().ok_or(KError::InvalidInput)?;
        (super_block, state.source.clone())
    };
    let namespace = kprocess::current_user_process().mnt_ns()?;
    let mount = namespace.create_detached_mount(
        &super_block,
        mount_attr_to_flags(attr_flags),
        source.as_deref(),
        None,
    )?;
    // The fsmount fd must be openable as the new mount's root directory so the
    // caller can populate it (systemd writes credentials) before move_mount
    // attaches it. Open the detached root and stash the mount for move_mount.
    let file = dentry_open(
        mount.root_path(),
        O_RDONLY | O_DIRECTORY,
        kprocess::current_cred(),
    )?;
    file.set_private_data(Arc::new(MountFd {
        mount: Mutex::new(Some(mount)),
    }));
    kprocess::current_files()
        .add_file(file, flags & FSMOUNT_CLOEXEC != 0)
        .map(|fd| fd as isize)
}

/// Attaches a detached mount at a target path (`move_mount(2)`).
///
/// # Execution context
///
/// Requires a current process thread: it reads the caller's fd table, mount
/// namespace and credentials. It may sleep (target pathname lookup and mount
/// graft), so it must not be called from IRQ context or while holding a
/// spinlock or IRQ-preemption guard.
pub fn sys_move_mount(
    from_fd: c_int,
    from_path: UserConstPtr<c_char>,
    to_fd: c_int,
    to_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let from_path = from_path.load_string_with_max_len(PATH_MAX)?;
    let to_path = to_path.load_string_with_max_len(PATH_MAX)?;
    debug!(
        "sys_move_mount <= from_fd: {from_fd}, to_fd: {to_fd}, to_path: {to_path:?}, flags: \
         {flags:#x}"
    );
    if flags & !MOVE_MOUNT_F_EMPTY_PATH != 0 {
        return Err(KError::InvalidInput);
    }
    if !from_path.is_empty() {
        // Moving an already-attached mount by path is not implemented; only
        // detached mounts from fsmount(2) (empty from_path) are supported.
        return Err(KError::InvalidInput);
    }
    // move_mount grafts a mount into the namespace; Linux requires CAP_SYS_ADMIN.
    let cred = kprocess::current_cred();
    require_mount_privilege(&cred)?;

    // Resolve the target before consuming the fsmount mount so a failed lookup
    // (ENOENT, invalid to_fd, ...) leaves the descriptor usable for a retry,
    // matching Linux, which looks up to_path before reading the from fd.
    //
    // Linux resolves to_path without following the final component unless the
    // caller passes MOVE_MOUNT_T_SYMLINKS; a symlink target is then refused by
    // the directory check instead of being mounted over. This path rejects
    // every flag except MOVE_MOUNT_F_EMPTY_PATH, so the no-follow lookup is the
    // only behaviour the accepted flag set can produce.
    let filename = Filename::new(to_path.as_str());
    let target = with_fs_at(to_fd, &filename, |fs| {
        filename.lookup_at(
            fs.root(),
            fs.pwd(),
            LookupIntent::Open,
            LookupFlags::no_follow(),
            &cred,
        )
    })?;
    let namespace = kprocess::current_user_process().mnt_ns()?;

    let from_file = kprocess::current_files().get_file(from_fd)?;
    let mfd = mount_fd(&from_file)?;
    // Hold the fsmount slot lock across take-attach-restore so a concurrent
    // move_mount on the same fd cannot observe the transient empty slot or
    // interleave with the rollback (CG-015: one critical section).
    let mut slot = mfd.mount.lock();
    let mount = slot.take().ok_or(KError::InvalidInput)?;
    // attach_detached_mount only fails before installing (registration / directory
    // checks), so on error restore the mount instead of permanently spending the fd.
    if let Err(error) = namespace.attach_detached_mount(mount.clone(), &target) {
        *slot = Some(mount);
        return Err(error);
    }
    Ok(0)
}
