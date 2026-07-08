// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P adapter utilities.

use alloc::string::String;

use kerrno::LinuxError;
use kvfs::{DeviceId, NodeType, VfsError};

/// Convert a 9P session error (String) into a VFS error.
///
/// The 9P `Session` methods return `Result<..., String>` where the error
/// string may contain a numeric errno (from `rlerror errno=N`) or a
/// human-readable message.
pub(crate) fn into_vfs_err(err: String) -> VfsError {
    // Try to parse "rlerror errno=N" format from RLERROR responses.
    if let Some(rest) = err.strip_prefix("rlerror errno=")
        && let Ok(errno) = rest.trim().parse::<i32>()
    {
        return VfsError::from(LinuxError::new(errno));
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

/// Decode a 9P2000.L st_rdev value with Linux new_decode_dev semantics.
pub(crate) fn dotl_decode_dev(encoded: u64) -> DeviceId {
    let encoded = encoded as u32;
    let major = (encoded & 0x000f_ff00) >> 8;
    let minor = (encoded & 0x0000_00ff) | ((encoded >> 12) & 0x000f_ff00);
    DeviceId::new(major, minor)
}

/// Convert a 9P readdir `d_type` to a VFS `NodeType`.
pub(crate) fn dtype_to_vfs(dtype: u8) -> NodeType {
    match dtype {
        1 => NodeType::Fifo,
        2 => NodeType::CharacterDevice,
        4 => NodeType::Directory,
        6 => NodeType::BlockDevice,
        8 => NodeType::RegularFile,
        10 => NodeType::Symlink,
        12 => NodeType::Socket,
        _ => NodeType::Unknown,
    }
}
