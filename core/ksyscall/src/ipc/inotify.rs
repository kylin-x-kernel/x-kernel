// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inotify syscalls.
//!
//! The instance object lives in `kfd_objects::inotify`; this module is the
//! syscall ABI adapter. `inotify_init1` therefore returns a real descriptor
//! instead of a sentinel, which matters because `0` names the caller's stdin
//! and a browser that treats it as an inotify instance will close or poll it.
//!
//! The adapter owns everything that depends on the caller's *environment*
//! rather than on the instance: the argument validation Linux performs before
//! it touches the descriptor, the pathname copy, and the resolution that turns
//! the argument into the canonical identity the watch is registered under.

use alloc::vec::Vec;
use core::ffi::c_char;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kfd_objects::inotify::{IN_ACCEPTED, INOTIFY_PATH_MAX, Inotify, WatchIdentity};
use linux_raw_sys::general::{
    IN_CLOEXEC, IN_DONT_FOLLOW, IN_MASK_ADD, IN_MASK_CREATE, IN_NONBLOCK, IN_ONLYDIR,
};
use posix_types::UserConstPtr;

bitflags! {
    /// Flags accepted by `inotify_init1`.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct InotifyInitFlags: u32 {
        /// Create a non-blocking instance.
        const NONBLOCK = IN_NONBLOCK;
        /// Set close-on-exec on the new descriptor.
        const CLOEXEC = IN_CLOEXEC;
    }
}

/// Creates an inotify instance and returns its new file descriptor.
///
/// Execution context: requires the current process thread (`current_cred`,
/// `current_files`), may sleep, and must not be called from interrupt context
/// or while holding a spin lock or a disabled-interrupt guard.
pub(crate) fn sys_inotify_init1(flags: u32) -> KResult<isize> {
    let flags = InotifyInitFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_inotify_init1 <= flags: {flags:?}");

    // As for `eventfd`: the anonymous inode file consumes only `O_NONBLOCK`,
    // and `O_CLOEXEC` is handed to `add_file` below so that
    // `OpenFlags::from_bits` does not have to know about it.
    let file = Inotify::new_file(flags.bits() & IN_NONBLOCK, kprocess::current_cred())?;
    kprocess::current_files()
        .add_file(file, flags.contains(InotifyInitFlags::CLOEXEC))
        .map(|fd| fd as isize)
}

/// Loads the watch path from user memory with a `PATH_MAX` bound.
///
/// `PATH_MAX` counts the terminator, so a payload of that many bytes is already
/// too long; Linux reports a pathname that exceeds the bound, or one with no
/// terminator inside it, as `ENAMETOOLONG` rather than `ERANGE`.
fn load_watch_path(path: UserConstPtr<c_char>) -> KResult<Vec<u8>> {
    path.load_bytes_with_max_len(INOTIFY_PATH_MAX - 1)
        .map_err(|err| {
            if err.canonicalize() == KError::OutOfRange {
                KError::NameTooLong
            } else {
                err
            }
        })
}

/// Resolves `fd` to its inotify instance.
///
/// Execution context: requires the current process thread (the lookup runs
/// against `current_files`), may sleep, and must not be called from interrupt
/// context or while holding a spin lock or a disabled-interrupt guard.
///
/// Linux reports a descriptor that is not an inotify instance as `EINVAL`, not
/// `EBADF`: the number is a valid descriptor, it just names the wrong object.
fn inotify_from_fd(fd: i32) -> KResult<alloc::sync::Arc<Inotify>> {
    let file = kprocess::current_files().get_file(fd)?;
    Inotify::from_file(&file).map_err(|err| {
        if err == KError::BadFileDescriptor {
            KError::InvalidInput
        } else {
            err
        }
    })
}

/// Adds a watch and returns its watch descriptor.
pub(crate) fn sys_inotify_add_watch(
    fd: i32,
    path: UserConstPtr<c_char>,
    mask: u32,
) -> KResult<isize> {
    // Linux validates the mask before the descriptor lookup and before the
    // pathname is copied: `inotify_add_watch(-1, NULL, 0)` is `EINVAL` there,
    // not `EBADF`. `IN_ACCEPTED` is Linux's whole `ALL_INOTIFY_BITS` domain, so
    // documented flags do not fail here, while an empty mask does — Linux's
    // second check is `!(mask & ALL_INOTIFY_BITS)` on a mask already proven to
    // lie inside that set, which is exactly `mask != 0`. The object repeats
    // those two rules for callers that bypass the adapter.
    if mask & !IN_ACCEPTED != 0 || mask == 0 {
        return Err(KError::InvalidInput);
    }
    let inotify = inotify_from_fd(fd)?;
    // Linux decides the two add directives are mutually exclusive *after*
    // `fdget` and before it checks the file's type, which is why it sits here
    // rather than next to the domain check: the fd is already known good, and
    // both remaining failures are `EINVAL` either way.
    if mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0 {
        return Err(KError::InvalidInput);
    }
    let path = load_watch_path(path)?;
    // Linux accepts only an existing, readable path here and then hangs the
    // watch on the resolved inode, which is what the identity below is: the
    // object keys watches by it, so a second call through another spelling of
    // the same object updates this watch instead of adding one. The device half
    // is the mount's synthetic id (`getattr`, not `metadata`), so mounts that
    // reuse an inode number do not share a watch.
    let cred = kprocess::current_cred();
    // `IN_ONLYDIR` is passed into the lookup so its `ENOTDIR` is decided before
    // the read-permission check, which is the order Linux applies.
    let target = posix_fs::watch::resolve_watch_target(
        &path,
        mask & IN_DONT_FOLLOW != 0,
        mask & IN_ONLYDIR != 0,
        &cred,
    )?;
    debug!(
        "sys_inotify_add_watch <= fd: {fd}, path_len: {}, mask: {mask:#x}",
        path.len()
    );

    let identity = WatchIdentity {
        device: target.device,
        inode: target.inode,
    };
    // The watch itself pins the object from here on, as an fsnotify mark does
    // in Linux: the inode cannot be freed -- and its number reused -- while any
    // watch names it, so no later revalidation is needed and none would be
    // correct (a concurrent rename would make a second lookup fail while the
    // object lives on).
    let pin = alloc::sync::Arc::clone(&target.inode_ref) as kfd_objects::inotify::Pin;
    let watch_descriptor = inotify.add_watch(mask, identity, pin)?;
    Ok(watch_descriptor as isize)
}

/// Removes the watch registered under `wd`.
pub(crate) fn sys_inotify_rm_watch(fd: i32, wd: i32) -> KResult<isize> {
    let inotify = inotify_from_fd(fd)?;
    debug!("sys_inotify_rm_watch <= fd: {fd}, wd: {wd}");

    inotify.remove_watch(wd).map(|()| 0)
}
