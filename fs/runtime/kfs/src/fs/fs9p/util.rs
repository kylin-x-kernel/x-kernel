// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P adapter utilities.

use alloc::string::String;

use kerrno::LinuxError;
use kvfs::{NodeType, VfsError};

/// Convert a 9P session error (String) into a VFS error.
///
/// The 9P `Session` methods return `Result<..., String>` where the error
/// string may contain a numeric errno (from `rlerror errno=N`) or a
/// human-readable message.
pub fn into_vfs_err(err: String) -> VfsError {
    // Try to parse "rlerror errno=N" format from RLERROR responses.
    if let Some(rest) = err.strip_prefix("rlerror errno=") {
        if let Ok(errno) = rest.trim().parse::<i32>() {
            return VfsError::from(LinuxError::new(errno));
        }
    }

    // Map well-known messages.
    let linux_error = if err.contains("not found") || err.contains("walk failed") {
        LinuxError::ENOENT
    } else if err.contains("not a directory") {
        LinuxError::ENOTDIR
    } else if err.contains("not initialized") || err.contains("not available") {
        LinuxError::ENODEV
    } else if err.contains("requires 9P2000.L") || err.contains("unsupported") {
        LinuxError::EOPNOTSUPP
    } else {
        LinuxError::EIO
    };

    VfsError::from(linux_error)
}

/// Convert a 9P qid type byte to a VFS `NodeType`.
///
/// Qid type bits:
///   0x80 = directory
///   0x02 = symlink
///   0x00 = regular file (default)
pub fn qid_type_to_vfs(qid_type: u8) -> NodeType {
    if qid_type & 0x80 != 0 {
        NodeType::Directory
    } else if qid_type & 0x02 != 0 {
        NodeType::Symlink
    } else {
        NodeType::RegularFile
    }
}

/// Convert a 9P readdir `d_type` to a VFS `NodeType`.
///
/// d_type values follow the Linux convention:
///   4 = directory, 8 = regular file, 10 = symlink, 0 = unknown.
pub fn dtype_to_vfs(dtype: u8) -> NodeType {
    match dtype {
        4 => NodeType::Directory,
        8 => NodeType::RegularFile,
        10 => NodeType::Symlink,
        _ => NodeType::Unknown,
    }
}
