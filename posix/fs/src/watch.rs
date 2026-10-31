// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path resolution for watch registration.
//!
//! `inotify_add_watch` registers an *object*, not a spelling: the watch is keyed
//! by the `(device, inode)` pair the path resolves to, because that is what the
//! event side reaches when it looks at the changed entry's parent. Registration
//! therefore resolves the caller's argument -- with the caller's credentials,
//! because Linux only lets a process watch an inode it may read.
//!
//! This lives in `posix_fs` rather than in the syscall adapter because it is
//! ordinary VFS path handling: the adapter has no business knowing how a
//! pathname is resolved, and no other crate can carry it without a dependency
//! cycle.

use kerrno::{KError, KResult};
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

/// What a resolved watch target is, for registration.
pub struct WatchTarget {
    /// Inode number at registration; half of the identity the watch is keyed
    /// by, and what `fdinfo` reports.
    pub inode: u64,
    /// Device number at registration — the mount's synthetic device id, so two
    /// mounts that reuse an inode number stay distinct. Bind mounts of one
    /// superblock carry different ids, so they register as two watches where
    /// Linux has one; see the deviation note below.
    pub device: u64,
    /// The resolved inode itself, held by the caller until the watch is
    /// registered.
    ///
    /// This pins the object across the resolution→registration window: without
    /// it the inode could be evicted in between, the eviction hook would find
    /// no watch to retire, and the registration that follows would sit on an
    /// identity that is already gone. Linux holds the same reference for the
    /// same reason.
    pub inode_ref: alloc::sync::Arc<kvfs::VfsInode>,
}

/// Resolves `path` for an inotify watch.
///
/// Linux only accepts an existing path here, which is why resolution failure
/// is surfaced instead of absorbed. `no_follow` carries the caller's
/// `IN_DONT_FOLLOW`: the watch then targets the link itself. The caller's
/// credentials drive both the search and the read permission checks.
///
/// Known deviations from Linux, both explicit rather than silent:
///
/// - the resolver takes `&str`, so a pathname that is not valid UTF-8 is
///   rejected with `EINVAL` rather than watched as an opaque byte string (Linux
///   resolves byte-wise, so such a name works there). Accepting it needs a
///   byte-oriented resolver in the VFS layer.
/// - the identity a watch is keyed by is `(mount, inode)`, not the inode object
///   itself: a bind mount of the same directory has its own mount id, so the
///   same object registered through the original and the bind path is two
///   watches where Linux has one. This kernel's `SuperBlock` exposes no stable
///   identifier to key on, so rather than invent one the granularity is stated
///   here.
/// - `IN_UNMOUNT`: Linux ORs `FS_UNMOUNT` into every mark, so a watch is always
///   subscribed to it here as well; the bit has no delivery source yet.
///
/// # Errors
///
/// Forwards the resolver's error for a path that cannot be resolved (a missing
/// entry is `NotFound`, as Linux reports `ENOENT`), and returns
/// `PermissionDenied` when the caller may not read the resolved object.
pub fn resolve_watch_target(
    path: &[u8],
    no_follow: bool,
    require_dir: bool,
    cred: &kcred::Cred,
) -> KResult<WatchTarget> {
    let path = core::str::from_utf8(path).map_err(|_| KError::InvalidInput)?;
    let flags = if no_follow { AT_SYMLINK_NOFOLLOW } else { 0 };
    let location =
        crate::path::resolve_at_with_cred(AT_FDCWD, Some(path), flags, cred)?.into_path()?;
    // `getattr` rather than `metadata`: the latter leaves `device` at 0 for the
    // filesystem's own attributes, while the former fills in the mount's
    // synthetic device id -- and the device is half of the identity a watch is
    // registered under, so two mounts whose root inodes share a number must not
    // collapse into one watch.
    // Linux: "you can only watch an inode if you have read permissions on it"
    // (`inotify_find_inode` calls `path_permission(path, MAY_READ)`), which is
    // what stops an unprivileged watcher from enumerating the entries of a
    // directory it may search but not read.
    // Order matters and follows Linux: the directory requirement is decided
    // right after resolution and *before* the permission check, so
    // `inotify_add_watch(fd, file, IN_ONLYDIR)` on an unreadable regular file
    // is `ENOTDIR` there, not `EACCES`. (Linux folds `IN_ONLYDIR` into the
    // lookup itself; this resolver has no directory flag, so the check happens
    // on the resolved location instead.)
    if require_dir && !location.is_dir() {
        return Err(KError::NotADirectory);
    }
    location
        .permission(kvfs::Permission::MAY_READ, cred)
        .map_err(|_| KError::PermissionDenied)?;
    let metadata = location.getattr()?;
    Ok(WatchTarget {
        inode: metadata.inode,
        device: metadata.device,
        inode_ref: location.inode(),
    })
}
