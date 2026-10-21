// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn special_record_lengths_follow_linux_encoding() {
    let zero = raw_directory_entry(0);
    let maximum = raw_directory_entry(u16::MAX);

    assert_eq!(zero.record_len(4_096), Ok(4_096));
    assert_eq!(maximum.record_len(4_096), Ok(4_096));
    assert_eq!(
        RawDirectoryEntry::encode_record_len(4_096, 4_096),
        Ok(4_096)
    );
    assert_eq!(
        RawDirectoryEntry::encode_record_len(65_536, 65_536),
        Ok(u16::MAX)
    );
}

#[def_test]
fn htree_root_distinguishes_disk_and_derived_hash_versions() {
    let mut bytes = [0; DX_ROOT_INFO_OFFSET + 8];
    bytes[DX_ROOT_INFO_OFFSET + 4] = DX_HASH_SIPHASH;
    bytes[DX_ROOT_INFO_OFFSET + 5] = 8;
    let root_info = HTreeRootInfo::decode(&bytes).expect("decode htree root info");
    assert_eq!(root_info.validate(), Ok(()));

    bytes[DX_ROOT_INFO_OFFSET + 4] = 5;
    let root_info = HTreeRootInfo::decode(&bytes).expect("decode htree root info");
    assert_eq!(
        root_info.validate(),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))
    );
}

fn raw_directory_entry(rec_len: u16) -> RawDirectoryEntry {
    let mut bytes = [0; DIRENT_HEADER_SIZE];
    bytes[4..6].copy_from_slice(&rec_len.to_le_bytes());
    RawDirectoryEntry::decode(&bytes, 0).expect("decode directory entry")
}
