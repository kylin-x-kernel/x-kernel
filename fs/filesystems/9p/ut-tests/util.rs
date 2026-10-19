// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
