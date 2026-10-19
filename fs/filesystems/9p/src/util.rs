// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P adapter utilities.

use kvfs::{DeviceId, NodeType};

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

unittest::path_mod_test!(tests, "../ut-tests/util.rs");
