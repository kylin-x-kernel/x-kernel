// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::ToOwned, string::String};
use core::ffi::c_int;

use fs_context::FsStruct;
use kerrno::{KError, KResult};
use kvfs::{Filename, LookupFlags, LookupIntent, Metadata, Path as VfsPath, path::Pathname};
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsStruct) -> KResult<R>) -> KResult<R> {
    if dirfd != AT_FDCWD {
        return Err(KError::InvalidInput);
    }

    // TEE file helpers are callable from both process and kernel-task paths,
    // so they must use the shared current-path filesystem view.
    let fs_struct = kprocess::current_user_process_fs_context();
    let mut fs = fs_struct.lock();
    f(&mut fs)
}

const TEE_FS_ROOT: &str = "/tee/";
const TEE_TMP_ROOT: &str = "/tmp/";

/// True when `normalized` is exactly `root` or a child path under `root/`.
fn is_under_root(normalized: &str, root: &str) -> bool {
    debug_assert!(root.ends_with('/'));
    let root_dir = root.trim_end_matches('/');
    normalized == root_dir || normalized.starts_with(root)
}

fn is_under_allowed_root(normalized: &str) -> bool {
    is_under_root(normalized, TEE_FS_ROOT) || is_under_root(normalized, TEE_TMP_ROOT)
}

fn has_parent_component(path: &str) -> bool {
    path.split('/').any(|component| component == "..")
}

/// Normalize and validate a path before TEE filesystem access.
///
/// Rejects `..` traversal and confines absolute paths to `/tee/` or `/tmp/`.
/// This is lexical validation only: a symlink under `/tee/` can still redirect
/// VFS lookup unless callers pass `AT_SYMLINK_NOFOLLOW`. `/tee/` is assumed
/// to be created by a trusted installer and not writable by untrusted TAs.
pub(crate) fn validate_tee_path(path: &str) -> KResult<String> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(KError::InvalidInput);
    }

    if has_parent_component(path) {
        return Err(KError::InvalidInput);
    }

    let normalized = Pathname::new(path)
        .normalize()
        .ok_or(KError::InvalidInput)?;
    let normalized = normalized.as_str();

    if !is_under_allowed_root(normalized) {
        return Err(KError::InvalidInput);
    }

    Ok(normalized.to_owned())
}

pub enum ResolveAtResult {
    File(VfsPath),
}

impl ResolveAtResult {
    pub fn stat(&self) -> KResult<Metadata> {
        match self {
            Self::File(file) => file.getattr(),
        }
    }
}

pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    let path = path
        .filter(|path| !path.is_empty())
        .ok_or(KError::NotFound)?;
    let path = validate_tee_path(path)?;
    let cred = kprocess::current_cred();

    with_fs(dirfd, |fs| {
        let lookup_flags = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            LookupFlags::no_follow()
        } else {
            LookupFlags::follow()
        };
        Filename::new(path.as_str())
            .lookup_at(fs.root(), fs.pwd(), LookupIntent::Stat, lookup_flags, &cred)
            .map(ResolveAtResult::File)
    })
}

unittest::path_mod_test!(tests, "../ut-tests/file/tests.rs");
