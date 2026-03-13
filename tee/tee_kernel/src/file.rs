// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_int;

use fs_ng_vfs::{Location, Metadata};
use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, FsContext};
use linux_raw_sys::general::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};

pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    if dirfd != AT_FDCWD {
        return Err(KError::InvalidInput);
    }

    let mut fs = FS_CONTEXT.lock();
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
