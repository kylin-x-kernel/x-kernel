// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, assert_ne, def_test};

use super::*;

#[def_test]
fn siphash_is_unsupported_instead_of_corrupt() {
    assert_eq!(
        DirectoryHash::from_effective_version(b"name", DX_HASH_SIPHASH, [0; 4]),
        Err(Ext4Error::Unsupported(UnsupportedKind::EncryptedName))
    );
}

#[def_test]
fn effective_version_selects_signedness() {
    let signed = DirectoryHash::from_effective_version(&[0x80], DX_HASH_LEGACY, [0; 4])
        .expect("signed legacy hash");
    let unsigned =
        DirectoryHash::from_effective_version(&[0x80], DX_HASH_LEGACY_UNSIGNED, [0; 4])
            .expect("unsigned legacy hash");

    assert_ne!(signed, unsigned);
}

#[def_test]
fn orlov_ignores_htree_default_version_and_unsigned_variant() {
    let seed = [1, 2, 3, 4];
    let orlov = DirectoryHash::orlov(&[0x80], seed);
    let signed = DirectoryHash::from_effective_version(&[0x80], DX_HASH_HALF_MD4, seed)
        .expect("signed half-MD4 hash");
    let unsigned =
        DirectoryHash::from_effective_version(&[0x80], DX_HASH_HALF_MD4_UNSIGNED, seed)
            .expect("unsigned half-MD4 hash");

    assert_eq!(orlov, signed);
    assert_ne!(orlov, unsigned);
    for htree_default_version in [DX_HASH_LEGACY, DX_HASH_TEA] {
        let htree_default =
            DirectoryHash::from_effective_version(&[0x80], htree_default_version, seed)
                .expect("HTree default hash");
        assert_ne!(orlov, htree_default);
    }
}
