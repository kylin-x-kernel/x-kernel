// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Path-link and namespace mutation syscalls.

use alloc::string::String;
use core::ffi::{c_char, c_int};

use kerrno::{KError, KResult};
use kvfs::{Filename, LookupIntent, RenameFlags};
use linux_raw_sys::general::*;
use posix_types::{UserConstPtr, UserPtr};

use crate::path::{resolve_at_with_cred, with_fs_at};

/// Creates a hard link to an existing file.
pub fn sys_linkat(
    old_dirfd: c_int,
    old_path: UserConstPtr<c_char>,
    new_dirfd: c_int,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_linkat <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    let supported_flags = AT_SYMLINK_FOLLOW | AT_EMPTY_PATH;
    if flags & !supported_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();

    let source_lookup_flags = if flags & AT_SYMLINK_FOLLOW != 0 {
        flags & !AT_SYMLINK_FOLLOW
    } else {
        flags | AT_SYMLINK_NOFOLLOW
    };
    let old = resolve_at_with_cred(old_dirfd, old_path.as_deref(), source_lookup_flags, &cred)?
        .into_path()?;
    let new_filename = Filename::new(new_path.as_str());
    with_fs_at(new_dirfd, &new_filename, |fs| {
        // `link_at` returns the path of the entry it just created; that is the
        // race-free identity of the object whose link count changed. Resolving
        // the name again would race a concurrent unlink/rename/create that
        // moved the name between the two lookups, sending `IN_ATTRIB` to
        // whatever now occupies it (or dropping the event when the lookup
        // fails) -- Linux reports the source inode directly.
        let created = new_filename.link_at_entry(fs.root(), fs.pwd(), &old, &cred)?;
        // The hard link is a new directory entry in the new directory, so the
        // creation is reported there and not where the original lives. The
        // linked inode's own link count also changed (Linux:
        // `fsnotify_link()` reports `FS_ATTRIB` to the inode's marks before the
        // directory's create), so its watches hear `IN_ATTRIB`.
        if crate::notify::entry_notify_wanted()
            && let Ok(metadata) = created.path.getattr()
        {
            crate::notify::notify_self_object(
                metadata.device,
                metadata.inode,
                crate::notify::IN_ATTRIB,
                false,
            );
        }
        crate::notify::notify_created_entry(&created, crate::notify::IN_CREATE);
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_link(old_path: UserConstPtr<c_char>, new_path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_linkat(AT_FDCWD, old_path, AT_FDCWD, new_path, 0)
}

/// Removes a directory entry.
pub fn sys_unlinkat(dirfd: i32, path: UserConstPtr<c_char>, flags: usize) -> KResult<isize> {
    let path = path.load_string()?;
    debug!("sys_unlinkat <= dirfd: {dirfd}, path: {path:?}, flags: {flags}");
    if flags & !(AT_REMOVEDIR as usize) != 0 {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        let is_dir = flags == AT_REMOVEDIR as usize;
        // The removal itself reports the victim's identity, captured inside the
        // namespace lock it holds. Resolving the path separately before the
        // call would let a concurrent rename/link/rebuild of the name hand the
        // event -- and the retirement -- to an object the unlink never touched.
        // The watch is still retired only after the removal succeeds: retiring
        // first would lose the watch for good on every failure path
        // (`ENOTEMPTY`, `EROFS`, `EBUSY`, `rmdir(".")`), reporting a removal
        // that never happened.
        let object_entry = if is_dir {
            filename.rmdir_at_tracked(fs.root(), fs.pwd(), &cred)?
        } else {
            filename.unlink_at_tracked(fs.root(), fs.pwd(), &cred)?
        };
        let object = if crate::notify::entry_notify_wanted() {
            object_entry.as_ref().map(|removed| {
                crate::notify::EntryObject::from_metadata(
                    removed.victim.clone(),
                    removed.victim_nlink.unwrap_or(1),
                )
            })
        } else {
            None
        };
        if let Some(object) = object
            && flags != AT_REMOVEDIR as usize
        {
            // Linux reports the link-count change to the inode's own watches
            // unconditionally (`vfs_unlink()` calls `fsnotify_link_count()`
            // even for the entry that loses its last link; `vfs_rmdir()` has
            // no such call, so a removed directory only hears the events
            // below). The object still exists at this point, so its watch --
            // however it is classified below -- is still registered and
            // receives this first.
            crate::notify::notify_self_object(
                object.device,
                object.inode,
                crate::notify::IN_ATTRIB,
                object.is_dir,
            );
        }
        // Report after the removal succeeded, so a watcher never sees
        // IN_DELETE for a call that failed. A removed directory carries
        // IN_ISDIR, as Linux reports it: the entry named was itself a
        // directory, which the flag on this call already says.
        let is_directory = if is_dir { crate::notify::IN_ISDIR } else { 0 };
        // The parent identity and the name come from the removal itself -- the
        // directory it locked and the entry it unlinked -- so the event lands in
        // the directory that really lost the entry.
        if let Some(removed) = &object_entry {
            crate::notify::notify_entry(
                removed.parent.device,
                removed.parent.inode,
                crate::notify::IN_DELETE | is_directory,
                removed.name.as_bytes(),
                0,
            );
        }
        if let Some(object) = object
            && object.disappears(is_dir)
        {
            // Retire last, matching Linux's observable order: the directory's
            // IN_DELETE is queued before the dying inode's FS_DELETE_SELF and
            // IN_IGNORED (which Linux delivers only when the inode is
            // evicted). Retiring only after the removal succeeded also keeps
            // the watch alive on every failure path (`ENOTEMPTY`, `EROFS`,
            // `EBUSY`, `rmdir(".")`), reporting a removal that never happened.
            crate::notify::retire_object(
                object.device,
                object.inode,
                crate::notify::IN_DELETE_SELF,
                object.is_dir,
            );
        }
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_rmdir(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, AT_REMOVEDIR as usize)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_unlink(path: UserConstPtr<c_char>) -> KResult<isize> {
    sys_unlinkat(AT_FDCWD, path, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_symlink(target: UserConstPtr<c_char>, linkpath: UserConstPtr<c_char>) -> KResult<isize> {
    sys_symlinkat(target, AT_FDCWD, linkpath)
}

/// Creates a symbolic link relative to a directory file descriptor.
pub fn sys_symlinkat(
    target: UserConstPtr<c_char>,
    new_dirfd: i32,
    linkpath: UserConstPtr<c_char>,
) -> KResult<isize> {
    let target = target.load_string()?;
    let linkpath = linkpath.load_string()?;
    debug!("sys_symlinkat <= target: {target:?}, new_dirfd: {new_dirfd}, linkpath: {linkpath:?}");
    let cred = kprocess::current_cred();
    let filename = Filename::new(linkpath.as_str());

    with_fs_at(new_dirfd, &filename, |fs| {
        let created = filename.symlink_at_entry(fs.root(), fs.pwd(), &target, &cred)?;
        crate::notify::notify_created_entry(&created, crate::notify::IN_CREATE);
        Ok(0)
    })
}

#[cfg(target_arch = "x86_64")]
/// Reads a symbolic link using the same signed-length ABI as [`sys_readlinkat`].
pub fn sys_readlink(path: UserConstPtr<c_char>, buf: UserPtr<u8>, size: i32) -> KResult<isize> {
    sys_readlinkat(AT_FDCWD, path, buf, size)
}

/// Reads the target of a symbolic link.
///
/// The syscall ABI takes a signed `int` length, unlike libc's `size_t` wrapper.
/// Returns the bytes copied, truncated to `size`, without a NUL terminator.
///
/// # Errors
///
/// Returns `KError::InvalidInput` for a non-positive size before accessing the
/// pathname. Path lookup and user-buffer write errors are propagated.
pub fn sys_readlinkat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    buf: UserPtr<u8>,
    size: i32,
) -> KResult<isize> {
    if size <= 0 {
        return Err(KError::InvalidInput);
    }
    let size = size as usize;
    let path = path.load_string()?;
    debug!("sys_readlinkat <= dirfd: {dirfd}, path: {path:?}");
    let cred = kprocess::current_cred();
    let filename = Filename::new(path.as_str());

    with_fs_at(dirfd, &filename, |fs| {
        let link = filename.readlink_at(fs.root(), fs.pwd(), &cred)?;
        let read = size.min(link.len());
        buf.write_vm_slice(&link.as_bytes()[..read])?;
        Ok(read as isize)
    })
}

unittest::path_mod_test!(readlink_tests, "../ut-tests/namei.rs");

#[cfg(target_arch = "x86_64")]
pub fn sys_rename(
    old_path: UserConstPtr<c_char>,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat(AT_FDCWD, old_path, AT_FDCWD, new_path)
}

pub fn sys_renameat(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
) -> KResult<isize> {
    sys_renameat2(old_dirfd, old_path, new_dirfd, new_path, 0)
}

/// One half of a move: the parent's `(device, inode)` and the entry name.
type MoveHalf = Option<((u64, u64), String)>;

/// Reports one completed rename to the watches that name its objects.
///
/// Split out of [`sys_renameat2`] so the syscall body stays readable; the order
/// here is the order Linux produces (`fsnotify_move()`, then the inode events).
fn report_rename(
    outcome: &kvfs::RenameOutcome,
    parts: Option<(MoveHalf, MoveHalf)>,
    moved_is_directory: bool,
    reverse_is_directory: bool,
    exchange: bool,
) {
    // The source never needs a link count (it only hears `IN_MOVE_SELF` and
    // supplies `IN_ISDIR`); the replaced target needs one only to decide
    // retirement.
    let from_object = outcome
        .source
        .clone()
        // The source never needs a link count: it only hears `IN_MOVE_SELF`
        // and supplies `IN_ISDIR`, so any value is inert here.
        .map(|meta| crate::notify::EntryObject::from_metadata(meta, 1));
    let to_object = outcome.replaced.clone().map(|meta| {
        crate::notify::EntryObject::from_metadata(meta, outcome.replaced_nlink.unwrap_or(1))
    });

    // Linux's order: the two move halves first (`fsnotify_move()` reports the
    // directories), then the moved object's own `IN_MOVE_SELF`.
    let (from_parts, to_parts) = parts.unzip();
    let from_parts = from_parts.flatten();
    let to_parts = to_parts.flatten();
    let reverse_from = to_parts.clone();
    let reverse_to = from_parts.clone();
    crate::notify::notify_rename_parts(from_parts, to_parts, moved_is_directory);
    if exchange {
        crate::notify::notify_rename_parts(reverse_from, reverse_to, reverse_is_directory);
    }

    // `fsnotify_move()` reports the replaced target's `FS_ATTRIB`
    // (`fsnotify_link_count(target)`) *before* the moved source's
    // `FS_MOVE_SELF`; the target's event goes first here too.
    if !exchange && let Some(object) = to_object {
        crate::notify::notify_self_object(
            object.device,
            object.inode,
            crate::notify::IN_ATTRIB,
            object.is_dir,
        );
    }
    if let Some(object) = from_object {
        // The object moved; it still exists, so its watches stay and only hear
        // `IN_MOVE_SELF`.
        crate::notify::notify_self_object(
            object.device,
            object.inode,
            crate::notify::IN_MOVE_SELF,
            object.is_dir,
        );
    }
    if exchange {
        // An exchange is two moves: the object that was at the new name hears
        // its own `IN_MOVE_SELF` too.
        if let Some(object) = to_object {
            crate::notify::notify_self_object(
                object.device,
                object.inode,
                crate::notify::IN_MOVE_SELF,
                object.is_dir,
            );
        }
    } else if let Some(object) = to_object {
        // The replaced object's watches are retired only if it really
        // disappears (a file with other links survives, as with unlink), and
        // retirement is last: Linux delivers `FS_DELETE_SELF` at inode
        // eviction, after everything `fsnotify_move()` reports.
        if object.disappears(false) {
            crate::notify::retire_object(
                object.device,
                object.inode,
                crate::notify::IN_DELETE_SELF,
                object.is_dir,
            );
        }
    }
}

pub fn sys_renameat2(
    old_dirfd: i32,
    old_path: UserConstPtr<c_char>,
    new_dirfd: i32,
    new_path: UserConstPtr<c_char>,
    flags: u32,
) -> KResult<isize> {
    let old_path = old_path.load_string()?;
    let new_path = new_path.load_string()?;
    debug!(
        "sys_renameat2 <= old_dirfd: {old_dirfd}, old_path: {old_path:?}, new_dirfd: {new_dirfd}, \
         new_path: {new_path}, flags: {flags}"
    );

    let flags = RenameFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    if flags.contains(RenameFlags::WHITEOUT) || flags.has_conflicting_modes() {
        return Err(KError::InvalidInput);
    }
    let cred = kprocess::current_cred();
    let old_filename = Filename::new(old_path.as_str());
    let new_filename = Filename::new(new_path.as_str());

    // Watches are the only reason to pay for any of the following. Both
    // halves of the move are attributed through the very `Path`s the rename
    // will operate on -- the parent lookup below is the rename's own, and the
    // entry identities come back from inside its namespace lock -- so a
    // concurrent namespace change between two lookups cannot make the event
    // describe a different object than the one that moved.
    let watchable = crate::notify::entry_notify_wanted();

    let (old_dir, old_name) = with_fs_at(old_dirfd, &old_filename, |fs| {
        old_filename
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open, &cred)
            .and_then(|lookup| lookup.into_normal())
    })?;
    let (new_dir, new_name) = with_fs_at(new_dirfd, &new_filename, |fs| {
        new_filename
            .parent_at(fs.root(), fs.pwd(), LookupIntent::Open, &cred)
            .and_then(|lookup| lookup.into_normal())
    })?;
    let parts = if watchable {
        let from = old_dir
            .getattr()
            .ok()
            .map(|meta| ((meta.device, meta.inode), old_name.clone()));
        let to = new_dir
            .getattr()
            .ok()
            .map(|meta| ((meta.device, meta.inode), new_name.clone()));
        Some((from, to))
    } else {
        None
    };

    // Everything below happens only once the rename has succeeded, which is
    // where Linux reports it too: a rename has plenty of deterministic failures
    // (`EEXIST` under RENAME_NOREPLACE, `EXDEV`, `ENOTEMPTY`, file-into-dir),
    // and a watcher must not be told about a move that did not happen -- nor
    // lose its watch to one.
    let exchange = flags.contains(RenameFlags::EXCHANGE);
    let outcome = old_dir.rename_tracked(&old_name, &new_dir, &new_name, flags, &cred)?;
    // Renaming a name onto itself, or between two links of one inode, changes
    // nothing: kvfs returns before its namespace callback runs, so `moved` is
    // false. Linux's `vfs_rename()` returns there too, ahead of
    // `fsnotify_move()`, so nothing may be reported -- the move halves were
    // captured before the call and would otherwise look like a real move.
    if !outcome.moved {
        return Ok(0);
    }
    let moved_is_directory = outcome
        .source
        .as_ref()
        .is_some_and(|meta| meta.mode.node_type() == kvfs::NodeType::Directory);
    let reverse_is_directory = outcome
        .replaced
        .as_ref()
        .is_some_and(|meta| meta.mode.node_type() == kvfs::NodeType::Directory);
    report_rename(
        &outcome,
        parts,
        moved_is_directory,
        reverse_is_directory,
        exchange,
    );
    Ok(0)
}
