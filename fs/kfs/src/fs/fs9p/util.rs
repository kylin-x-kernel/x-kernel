// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9p adapter utilities.
use alloc::string::String;

use fs_ng_vfs::VfsError;
use kerrno::LinuxError;

pub fn into_vfs_err(err: String) -> VfsError {
    let lower = err.to_ascii_lowercase();
    if let Some(errno_str) = lower.strip_prefix("rlerror errno=") {
        if let Ok(errno) = errno_str.trim().parse::<i32>() {
            return VfsError::from(LinuxError::new(errno)).canonicalize();
        }
    }

    let linux_error = if lower.contains("not a directory") {
        LinuxError::ENOTDIR
    } else if lower.contains("is a directory") {
        LinuxError::EISDIR
    } else if lower.contains("walk failed")
        || lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("does not exist")
    {
        LinuxError::ENOENT
    } else if lower.contains("unsupported") {
        LinuxError::EOPNOTSUPP
    } else if lower.contains("permission") {
        LinuxError::EACCES
    } else {
        LinuxError::EIO
    };
    VfsError::from(linux_error).canonicalize()
}
