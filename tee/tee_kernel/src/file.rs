// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_int;

use kerrno::{KError, KResult};
use kfs::FsContext;
use kvfs::{Location, Metadata};
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    if dirfd != AT_FDCWD {
        return Err(KError::InvalidInput);
    }

    // TEE file helpers are callable from both process and kernel-task paths,
    // so they must use the shared current-path filesystem view.
    let fs_context = kthread::current_fs_context();
    let mut fs = fs_context.lock();
    f(&mut fs)
}

pub enum ResolveAtResult {
    File(Location),
}

impl ResolveAtResult {
    pub fn stat(&self) -> KResult<Metadata> {
        match self {
            Self::File(file) => file.metadata(),
        }
    }
}

pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    let path = path
        .filter(|path| !path.is_empty())
        .ok_or(KError::NotFound)?;

    with_fs(dirfd, |fs| {
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path)
        } else {
            fs.resolve(path)
        }
        .map(ResolveAtResult::File)
    })
}
