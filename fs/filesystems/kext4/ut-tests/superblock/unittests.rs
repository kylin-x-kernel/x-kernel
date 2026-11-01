// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

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

fn interleaved_journal_filesystem() -> Ext4SbInfo {
    use super::test_fixtures::*;

    let group = AllocatorGroupSpec {
        free_blocks: 0,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff; 4],
        inode_bitmap: [0; 4],
    };
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[group; 40]);
    let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    let superblock_bytes = &mut bytes[offset..offset + len];
    let compat = le_u32(superblock_bytes, 0x5c) | features::CompatFeatures::HAS_JOURNAL.bits();
    put_u32(superblock_bytes, 0x5c, compat);
    put_u32(superblock_bytes, 0xe0, 8);
    filesystem.superblock = Ext4DiskSuperblock::decode(superblock_bytes).unwrap();
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();

    let mut extents = Vec::new();
    let mut logical_start = 0;
    for group in 0u32..40 {
        let len = (1024 - logical_start).min(26);
        let physical_start = group * 32 + 6;
        extents.push(JournalExtent {
            logical_start,
            physical_start: u64::from(physical_start),
            len,
        });
        let block_count = filesystem.layout().block_count();
        add_system_zone_to(
            &mut filesystem.system_zones,
            u64::from(physical_start),
            u64::from(len),
            Some(InodeNumber::new(8)),
            block_count,
        )
        .unwrap();
        logical_start += len;
    }
    filesystem.journal = Some(
        MountedJournal::new(
            InternalJournal {
                superblock: JournalSuperblock::decode(
                    &test_journal_superblock_bytes_with_blocks(200, 1024),
                    TEST_BLOCK_SIZE as u32,
                    1024,
                    filesystem.superblock().uuid(),
                )
                .unwrap(),
                extents,
                block_count: 1024,
            },
            filesystem.layout().block_count(),
        )
        .unwrap(),
    );
    filesystem
}

#[def_test]
fn recovery_refresh_keeps_interleaved_journal_blocks_protected() {
    let mut filesystem = interleaved_journal_filesystem();
    for should_discard_previous_index in [false, false, true] {
        if should_discard_previous_index {
            filesystem.system_zones.clear();
        }
        filesystem.refresh_recovered_superblock().unwrap();
        for logical in [0, 25, 26, 1023] {
            let physical = filesystem
                .map_journal_block(JournalBlock::new(logical))
                .unwrap();
            assert!(filesystem.is_system_zone_block(physical));
            assert!(filesystem.is_inode_physical_block_valid(
                InodeNumber::new(8),
                physical.get(),
                1
            ));
            assert!(!filesystem.is_inode_physical_block_valid(
                InodeNumber::new(12),
                physical.get(),
                1
            ));
        }
        assert!(filesystem.is_system_zone_block(FilesystemBlock::new(34)));
        assert!(!filesystem.is_inode_physical_block_valid(InodeNumber::new(8), 34, 1));
    }
}

#[def_test]
fn recovery_refresh_rejects_journal_ranges_overlapping_metadata() {
    let mut filesystem = interleaved_journal_filesystem();
    filesystem.journal = Some(
        MountedJournal::new(
            InternalJournal {
                superblock: filesystem.journal.as_ref().unwrap().superblock(),
                extents: vec![JournalExtent {
                    logical_start: 0,
                    physical_start: 2,
                    len: 1024,
                }],
                block_count: 1024,
            },
            filesystem.layout().block_count(),
        )
        .unwrap(),
    );
    assert_eq!(
        filesystem.refresh_recovered_superblock(),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry)),
    );
}

#[def_test]
fn recovery_refresh_rejects_a_changed_journal_backing_identity() {
    use super::test_fixtures::{TEST_BLOCK_SIZE, put_u32};

    let mut filesystem = interleaved_journal_filesystem();
    let (block, offset, _) = filesystem.primary_superblock_location().unwrap();
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    put_u32(&mut bytes, offset + 0xe0, 9);
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();

    assert_eq!(
        filesystem.refresh_recovered_superblock(),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal)),
    );
}
