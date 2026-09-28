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

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn dotl_device_number_uses_linux_encoding() {
        let major = 0xabc_u32;
        let minor = 0xdef01_u32;
        let encoded = ((major & 0xfff) << 8) | (minor & 0xff) | ((minor & !0xff) << 12);

        let device = dotl_decode_dev(u64::from(encoded));

        assert_eq!(device.major(), major);
        assert_eq!(device.minor(), minor);
    }

    #[def_test]
    fn dotl_directory_entry_types_map_to_vfs_types() {
        assert_eq!(dtype_to_vfs(1), NodeType::Fifo);
        assert_eq!(dtype_to_vfs(2), NodeType::CharacterDevice);
        assert_eq!(dtype_to_vfs(4), NodeType::Directory);
        assert_eq!(dtype_to_vfs(6), NodeType::BlockDevice);
        assert_eq!(dtype_to_vfs(8), NodeType::RegularFile);
        assert_eq!(dtype_to_vfs(10), NodeType::Symlink);
        assert_eq!(dtype_to_vfs(12), NodeType::Socket);
        assert_eq!(dtype_to_vfs(0xff), NodeType::Unknown);
    }
}
