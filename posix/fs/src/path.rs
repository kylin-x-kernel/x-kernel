// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux path-resolution helpers for filesystem syscalls and exec.

use core::ffi::c_int;

use fs_context::FsStruct;
use kcred::Cred;
use kerrno::{KError, KResult};
use kfd::Kstat;
use kprocess::current_user_process;
use kvfs::{Filename, LookupFlags, LookupIntent, Path};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};

/// Executes a function with the file system context for the given directory file descriptor.
///
/// If `dirfd` is `AT_FDCWD`, uses the current directory context.
/// Otherwise, resolves the directory from the given file descriptor and uses it as the base.
pub(crate) fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&FsStruct) -> KResult<R>) -> KResult<R> {
    let fs_struct = kprocess::current_user_process_fs_context();
    let fs = fs_struct.lock().snapshot();
    let snapshot = if dirfd == AT_FDCWD {
        fs
    } else {
        let file = kprocess::current_resources().get_file(dirfd)?;
        if !file.is_dir() {
            return Err(KError::NotADirectory);
        }
        let dir = file.path().clone();
        fs.clone_with_pwd(dir)?
    };
    f(&snapshot)
}

/// Executes a function with the filesystem context selected for an `*at` pathname.
///
/// Absolute pathnames are resolved from the process root and therefore do not
/// inspect `dirfd`. Relative pathnames use the directory selected by `dirfd`.
pub(crate) fn with_fs_at<R>(
    dirfd: c_int,
    filename: &Filename,
    f: impl FnOnce(&FsStruct) -> KResult<R>,
) -> KResult<R> {
    with_fs(at_path_dirfd(dirfd, filename), f)
}

fn at_path_dirfd(dirfd: c_int, filename: &Filename) -> c_int {
    if filename.as_pathname().is_absolute() {
        AT_FDCWD
    } else {
        dirfd
    }
}

/// Result of resolving a path at a given directory.
#[derive(Clone)]
pub(crate) enum ResolveAtResult {
    Path(Path),
}

impl ResolveAtResult {
    pub(crate) fn into_path(self) -> KResult<Path> {
        match self {
            Self::Path(location) => Ok(location),
        }
    }

    pub(crate) fn stat(&self) -> KResult<Kstat> {
        match self {
            Self::Path(location) => location.getattr().map(Kstat::from),
        }
    }
}

fn resolve_empty_path(dirfd: c_int, flags: u32) -> KResult<ResolveAtResult> {
    if flags & AT_EMPTY_PATH == 0 {
        return Err(KError::NotFound);
    }
    let resources = current_user_process().resources()?;
    let file = resources.get_file(dirfd)?;
    Ok(ResolveAtResult::Path(file.path().clone()))
}

fn resolve_filesystem_path(dirfd: c_int, path: &str, flags: u32, cred: &Cred) -> KResult<Path> {
    let filename = Filename::new(path);
    with_fs_at(dirfd, &filename, |fs| {
        let lookup_flags = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            LookupFlags::no_follow()
        } else {
            LookupFlags::follow()
        };
        filename.lookup_at(fs.root(), fs.pwd(), LookupIntent::Stat, lookup_flags, cred)
    })
}

pub(crate) fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    let cred = kprocess::current_cred();
    resolve_at_with_cred(dirfd, path, flags, &cred)
}

pub(crate) fn resolve_at_with_cred(
    dirfd: c_int,
    path: Option<&str>,
    flags: u32,
    cred: &Cred,
) -> KResult<ResolveAtResult> {
    match path {
        Some(path) if !path.is_empty() => {
            resolve_filesystem_path(dirfd, path, flags, cred).map(ResolveAtResult::Path)
        }
        _ => resolve_empty_path(dirfd, flags),
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn absolute_at_path_does_not_select_dirfd() {
        assert_eq!(at_path_dirfd(1234, &Filename::new("/absolute")), AT_FDCWD);
        assert_eq!(at_path_dirfd(1234, &Filename::new("relative")), 1234);
    }
}
