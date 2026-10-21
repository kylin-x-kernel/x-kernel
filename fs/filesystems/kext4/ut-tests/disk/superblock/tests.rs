// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    CHECKSUM_OFFSET, CRC32C_CHECKSUM_TYPE, EXT2_FLAGS_UNSIGNED_HASH, Ext4DiskSuperblock,
    LINUX_DEFAULT_EXTRA_ISIZE, MIN_EXTRA_ISIZE_OFFSET, SUPERBLOCK_SIZE,
    WANT_EXTRA_ISIZE_OFFSET,
};
use crate::{ChecksumTarget, CorruptKind, Ext4Error, disk::checksum};

const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_64BIT: u32 = 0x0080;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const RO_COMPAT_BIGALLOC: u32 = 0x0200;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

#[def_test]
fn derives_linux_default_effective_want_extra_isize() {
    let decoded = Ext4DiskSuperblock::decode(&valid_superblock()).unwrap();

    assert_eq!(decoded.min_extra_isize(), 0);
    assert_eq!(decoded.want_extra_isize(), LINUX_DEFAULT_EXTRA_ISIZE);
}

#[def_test]
fn effective_want_extra_isize_includes_disk_requirements() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x64, RO_COMPAT_EXTRA_ISIZE);
    put_u16(&mut bytes, MIN_EXTRA_ISIZE_OFFSET, 48);
    put_u16(&mut bytes, WANT_EXTRA_ISIZE_OFFSET, 64);

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();

    assert_eq!(decoded.min_extra_isize(), 48);
    assert_eq!(decoded.want_extra_isize(), 64);
}

#[def_test]
fn effective_want_extra_isize_ignores_disk_fields_without_feature() {
    let mut bytes = valid_superblock();
    put_u16(&mut bytes, MIN_EXTRA_ISIZE_OFFSET, 48);
    put_u16(&mut bytes, WANT_EXTRA_ISIZE_OFFSET, 64);

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();

    assert_eq!(decoded.min_extra_isize(), 0);
    assert_eq!(decoded.want_extra_isize(), LINUX_DEFAULT_EXTRA_ISIZE);
}

#[def_test]
fn accepts_linux_64bit_descriptor_geometry() {
    for descriptor_size in [64, 128, 1024] {
        let mut bytes = valid_superblock();
        put_u16(&mut bytes, 0xfe, descriptor_size);
        assert!(Ext4DiskSuperblock::decode(&bytes).is_ok());
    }
}

#[def_test]
fn rejects_non_power_of_two_64bit_descriptor_sizes() {
    for descriptor_size in [32, 40, 48, 56, 96, 1025] {
        let mut bytes = valid_superblock();
        put_u16(&mut bytes, 0xfe, descriptor_size);
        assert_eq!(
            Ext4DiskSuperblock::decode(&bytes),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidDescriptorSize))
        );
    }
}

#[def_test]
fn rejects_first_data_block_outside_filesystem() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x14, 32_768);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
    );
}

#[def_test]
fn rejects_zero_first_data_block_for_one_kib_blocks() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x18, 0);
    put_u32(&mut bytes, 0x1c, 0);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
    );
}

#[def_test]
fn rejects_group_geometry_larger_than_bitmap() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x20, 32_769);
    put_u32(&mut bytes, 0x24, 32_769);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
    );
}

#[def_test]
fn rejects_cluster_geometry_larger_than_bitmap() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x24, 32_769);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidClusterGeometry))
    );
}

#[def_test]
fn rejects_bigalloc_before_cluster_semantics_can_be_mounted() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x64, RO_COMPAT_BIGALLOC);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::UnsupportedFeature {
            class: crate::FeatureClass::ReadOnlyCompatible,
            bits: RO_COMPAT_BIGALLOC,
        })
    );
}

#[def_test]
fn decodes_non_extent_superblock_before_mount_negotiation() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x60, 0);

    assert!(Ext4DiskSuperblock::decode(&bytes).is_ok());
}

#[def_test]
fn checksum_failure_precedes_mount_feature_negotiation() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
    bytes[0x175] = CRC32C_CHECKSUM_TYPE;
    update_superblock_checksum(&mut bytes);

    // Corrupt the checksummed feature word so the filesystem also appears
    // to lack extents. Integrity failure must win over mount capability.
    put_u32(&mut bytes, 0x60, INCOMPAT_64BIT);

    assert!(matches!(
        Ext4DiskSuperblock::decode(&bytes),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::Superblock,
            ..
        })
    ));
}

#[def_test]
fn rejects_invalid_inode_geometry() {
    let mut too_few = valid_superblock();
    put_u32(&mut too_few, 0x00, 8);
    put_u32(&mut too_few, 0x28, 8);
    assert_eq!(
        Ext4DiskSuperblock::decode(&too_few),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
    );

    let mut too_many = valid_superblock();
    put_u32(&mut too_many, 0x00, 32_769);
    put_u32(&mut too_many, 0x28, 32_769);
    assert_eq!(
        Ext4DiskSuperblock::decode(&too_many),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
    );

    let mut count_mismatch = valid_superblock();
    put_u32(&mut count_mismatch, 0x00, 8191);
    assert_eq!(
        Ext4DiskSuperblock::decode(&count_mismatch),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
    );
}

#[def_test]
fn decodes_and_updates_legacy_orphan_head() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, super::LAST_ORPHAN_OFFSET, 12);

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();
    assert_eq!(decoded.last_orphan(), 12);

    let updated = super::set_last_orphan(&mut bytes, 13).unwrap();
    assert_eq!(updated.last_orphan(), 13);
    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes).unwrap().last_orphan(),
        13
    );
}

#[def_test]
fn preserves_directory_hash_flags_as_disk_facts() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x160, EXT2_FLAGS_UNSIGNED_HASH);

    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes)
            .expect("decode unsigned-hash superblock")
            .flags(),
        EXT2_FLAGS_UNSIGNED_HASH
    );
}

#[def_test]
fn setting_unsigned_hash_flag_updates_metadata_checksum() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
    bytes[0x175] = CRC32C_CHECKSUM_TYPE;
    update_superblock_checksum(&mut bytes);

    let updated = super::set_unsigned_hash_flag(&mut bytes).unwrap();

    assert_eq!(
        updated.flags() & EXT2_FLAGS_UNSIGNED_HASH,
        EXT2_FLAGS_UNSIGNED_HASH
    );
    assert_eq!(Ext4DiskSuperblock::decode(&bytes).unwrap(), updated);
}

#[def_test]
fn decodes_out_of_range_orphan_head_for_recovery() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, super::LAST_ORPHAN_OFFSET, 8193);

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();
    assert_eq!(decoded.last_orphan(), 8193);
    assert_eq!(
        super::set_last_orphan(&mut bytes, 8193),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber))
    );
    let cleared = super::set_last_orphan(&mut bytes, 0).unwrap();
    assert_eq!(cleared.last_orphan(), 0);
}

#[def_test]
fn clears_recovery_feature_without_metadata_checksum() {
    let mut bytes = valid_superblock();
    put_u32(
        &mut bytes,
        0x60,
        INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_RECOVER,
    );

    super::clear_ext4_needs_recovery_feature(&mut bytes).unwrap();

    assert_eq!(
        Ext4DiskSuperblock::decode(&bytes)
            .unwrap()
            .features()
            .incompat()
            .bits()
            & INCOMPAT_RECOVER,
        0
    );
}

#[def_test]
fn sets_recovery_feature_without_metadata_checksum() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);

    super::set_ext4_needs_recovery_feature(&mut bytes).unwrap();

    assert_ne!(
        Ext4DiskSuperblock::decode(&bytes)
            .unwrap()
            .features()
            .incompat()
            .bits()
            & INCOMPAT_RECOVER,
        0
    );
}

#[def_test]
fn clears_recovery_feature_and_updates_metadata_checksum() {
    let mut bytes = valid_superblock();
    put_u32(
        &mut bytes,
        0x60,
        INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_RECOVER,
    );
    put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
    bytes[0x175] = CRC32C_CHECKSUM_TYPE;
    update_superblock_checksum(&mut bytes);

    super::clear_ext4_needs_recovery_feature(&mut bytes).unwrap();

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();
    assert_eq!(decoded.features().incompat().bits() & INCOMPAT_RECOVER, 0);
}

#[def_test]
fn sets_recovery_feature_and_updates_metadata_checksum() {
    let mut bytes = valid_superblock();
    put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);
    put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
    bytes[0x175] = CRC32C_CHECKSUM_TYPE;
    update_superblock_checksum(&mut bytes);

    super::set_ext4_needs_recovery_feature(&mut bytes).unwrap();

    let decoded = Ext4DiskSuperblock::decode(&bytes).unwrap();
    assert_ne!(decoded.features().incompat().bits() & INCOMPAT_RECOVER, 0);
}

fn valid_superblock() -> [u8; SUPERBLOCK_SIZE] {
    let mut bytes = [0; SUPERBLOCK_SIZE];
    put_u32(&mut bytes, 0x00, 8192);
    put_u32(&mut bytes, 0x04, 32_768);
    put_u32(&mut bytes, 0x14, 0);
    put_u32(&mut bytes, 0x18, 2);
    put_u32(&mut bytes, 0x1c, 2);
    put_u32(&mut bytes, 0x20, 32_768);
    put_u32(&mut bytes, 0x24, 32_768);
    put_u32(&mut bytes, 0x28, 8192);
    put_u16(&mut bytes, 0x38, 0xef53);
    put_u32(&mut bytes, 0x4c, 1);
    put_u32(&mut bytes, 0x54, 11);
    put_u16(&mut bytes, 0x58, 256);
    put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);
    put_u16(&mut bytes, 0xfe, 64);
    bytes
}

fn update_superblock_checksum(bytes: &mut [u8]) {
    let checksum = checksum::crc32c(u32::MAX, &bytes[..CHECKSUM_OFFSET]);
    put_u32(bytes, CHECKSUM_OFFSET, checksum);
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
