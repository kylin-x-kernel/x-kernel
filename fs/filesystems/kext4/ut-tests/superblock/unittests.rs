// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn mount_hash_policy_matches_linux_signedness_order() {
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, false, 0),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, false, EXT2_FLAGS_SIGNED_HASH),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, false, EXT2_FLAGS_UNSIGNED_HASH),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(
            0,
            false,
            EXT2_FLAGS_SIGNED_HASH | EXT2_FLAGS_UNSIGNED_HASH,
        ),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, true, 0),
        Ok(DX_HASH_UNSIGNED_OFFSET)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, true, EXT2_FLAGS_SIGNED_HASH),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(0, true, EXT2_FLAGS_UNSIGNED_HASH),
        Ok(DX_HASH_UNSIGNED_OFFSET)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(
            0,
            true,
            EXT2_FLAGS_SIGNED_HASH | EXT2_FLAGS_UNSIGNED_HASH,
        ),
        Ok(DX_HASH_UNSIGNED_OFFSET)
    );
}

#[def_test]
fn mount_hash_policy_rejects_invalid_default_hash() {
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(5, false, 0),
        Ok(0)
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(DX_HASH_SIPHASH, false, 0),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryHash))
    );
    assert_eq!(
        Ext4SbInfo::resolve_directory_hash_unsigned(u8::MAX, true, 0),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryHash))
    );
}
