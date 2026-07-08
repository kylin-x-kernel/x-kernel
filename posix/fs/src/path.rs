// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux path-resolution helpers for filesystem syscalls and exec.

use alloc::sync::Arc;
use core::ffi::c_int;

use kerrno::{KError, KResult};
use kfd::{FileLike, Kstat};
use kfs::{File, FsContext};
use kvfs::{Location, LookupFlags, LookupIntent, lookup_location};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};

/// Executes a function with the file system context for the given directory file descriptor.
///
/// If `dirfd` is `AT_FDCWD`, uses the current directory context.
/// Otherwise, resolves the directory from the given file descriptor and uses it as the base.
pub(crate) fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    let fs_context = kprocess::current_fs_context();
    let mut fs = fs_context.lock();
    if dirfd == AT_FDCWD {
        f(&mut fs)
    } else {
        let dir = kprocess::current_resources().get_file_like_as::<File>(dirfd)?;
        dir.check_is_dir()?;
        let dir = dir.location().clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

/// Result of resolving a path at a given directory.
#[derive(Clone)]
pub(crate) enum ResolveAtResult {
    Location(Location),
    Other(Arc<dyn FileLike>),
}

impl ResolveAtResult {
    pub(crate) fn into_location(self) -> KResult<Location> {
        match self {
            Self::Location(location) => Ok(location),
            Self::Other(_) => Err(KError::BadFileDescriptor),
        }
    }

    pub(crate) fn stat(&self) -> KResult<Kstat> {
        match self {
            Self::Location(location) => location.metadata().map(Kstat::from),
            Self::Other(file_like) => file_like.stat(),
        }
    }
}

fn resolve_result_from_file_like(file_like: Arc<dyn FileLike>) -> KResult<ResolveAtResult> {
    if let Some(location) = file_like.vfs_location() {
        Ok(ResolveAtResult::Location(location))
    } else {
        Ok(ResolveAtResult::Other(file_like))
    }
}

fn resolve_empty_path(dirfd: c_int, flags: u32) -> KResult<ResolveAtResult> {
    if flags & AT_EMPTY_PATH == 0 {
        return Err(KError::NotFound);
    }
    let file_like = kprocess::current_resources().get_file_like(dirfd)?;
    resolve_result_from_file_like(file_like)
}

fn resolve_filesystem_path(dirfd: c_int, path: &str, flags: u32) -> KResult<Location> {
    with_fs(dirfd, |fs| {
        let lookup_flags = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            LookupFlags::no_follow()
        } else {
            LookupFlags::follow()
        };
        lookup_location(&fs.lookup_context(), path, LookupIntent::Stat, lookup_flags)
    })
}

pub(crate) fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    match path {
        Some(path) if !path.is_empty() => {
            resolve_filesystem_path(dirfd, path, flags).map(ResolveAtResult::Location)
        }
        _ => resolve_empty_path(dirfd, flags),
    }
}
