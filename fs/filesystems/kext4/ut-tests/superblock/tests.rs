// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{sync::Arc, vec};
use core::num::NonZeroU32;

use unittest::def_test;

use super::*;
use crate::{
    BlockCount, BlockGroupNumber, BlockMapping, CorruptKind, Ext4DiskSuperblock, Ext4Error,
    InodeNumber, PhysicalBlock, UnsupportedKind,
    dirhash::DX_HASH_UNSIGNED_OFFSET,
    disk::{features, superblock},
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::{
        Ext4SbInfo, InternalJournal,
        test_fixtures::{
            AllocatorGroupSpec, TEST_BLOCK_COUNT, TEST_BLOCK_SIZE, TEST_FREE_BLOCKS,
            TEST_FREE_INODES, TestDevice, allocate_checkpointed_regular_inode,
            allocator_multigroup_test_filesystem, allocator_superblock, allocator_test_filesystem,
            le_u32, persist_test_orphan_head, put_u32, set_allocator_default_hash_version,
            set_allocator_feature_bits, test_journal_superblock,
        },
    },
};

#[def_test]
fn mount_rejects_non_extent_after_superblock_decode() {
    let mut image = vec![0; TEST_BLOCK_SIZE * TEST_BLOCK_COUNT];
    let mut superblock_bytes = allocator_superblock(TEST_FREE_BLOCKS, TEST_FREE_INODES);
    put_u32(
        &mut superblock_bytes,
        0x60,
        features::IncompatFeatures::BIT_64.bits(),
    );
    image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);
    let device: Arc<block::BlockDevice> =
        crate::io::test_block_device(Arc::new(TestDevice::new(image)));

    assert!(matches!(
        Ext4SbInfo::mount(device),
        Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentFilesystem))
    ));
}

#[def_test]
fn mutable_state_reload_revalidates_mount_features() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    {
        let mut bytes = device.bytes.lock();
        let superblock_bytes = &mut bytes[1024..1024 + superblock::SUPERBLOCK_SIZE];
        put_u32(
            superblock_bytes,
            0x60,
            features::IncompatFeatures::BIT_64.bits(),
        );
    }
    filesystem.metadata_io.invalidate_all();

    assert_eq!(
        filesystem.reload_mutable_metadata_state(),
        Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentFilesystem))
    );
}

#[def_test]
fn indexed_hash_policy_initialization_does_not_persist() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::DIR_INDEX,
        features::ReadOnlyCompatFeatures::empty(),
    );

    filesystem.initialize_directory_hash_policy().unwrap();

    assert_eq!(filesystem.hash_unsigned, DX_HASH_UNSIGNED_OFFSET);
    assert_eq!(filesystem.superblock().flags(), 0);
    assert_eq!(device.flush_count(), 0);
}

#[def_test]
fn writable_mount_finalization_persists_unsigned_default() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::DIR_INDEX,
        features::ReadOnlyCompatFeatures::empty(),
    );

    filesystem.initialize_directory_hash_policy().unwrap();

    assert_eq!(filesystem.superblock().flags(), 0);
    assert_eq!(device.flush_count(), 0);

    filesystem.persist_directory_hash_policy().unwrap();

    assert_eq!(filesystem.hash_unsigned, DX_HASH_UNSIGNED_OFFSET);
    assert_ne!(
        filesystem.superblock().flags() & EXT2_FLAGS_UNSIGNED_HASH,
        0
    );
    assert_ne!(
        le_u32(&device.bytes(), 1024 + 0x160) & EXT2_FLAGS_UNSIGNED_HASH,
        0
    );
    assert_eq!(device.flush_count(), 1);
}

#[def_test]
fn reload_rejects_replayed_descriptor_address_changes() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);

    // Unchanged descriptors reload into the same mutable state.
    filesystem.metadata_io.invalidate_all();
    filesystem
        .reload_mutable_metadata_state()
        .expect("reload accepts an unchanged descriptor table");

    // A replayed descriptor table that moved a bitmap address must not
    // split the frozen geometry table from the reloaded allocator state.
    //
    // The test image has metadata_csum off and first_data_block 0, so
    // rewriting the block-bitmap lo-u32's low byte from 2 to 5 keeps the
    // descriptor valid: decode skips its checksum, block 5 stays inside
    // group 0 for `validate_group`, and only the frozen-geometry
    // comparison rejects it.
    device.bytes.lock()[TEST_BLOCK_SIZE] = 5;
    filesystem.metadata_io.invalidate_all();
    assert_eq!(
        filesystem.reload_mutable_metadata_state(),
        Err(Ext4Error::Corrupt(
            CorruptKind::GroupDescriptorAddressChanged
        ))
    );
    assert_eq!(filesystem.groups()[0].free_blocks_count(), TEST_FREE_BLOCKS);
}

#[def_test]
fn mount_rejects_clean_legacy_orphan_head_without_recovery() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    persist_test_orphan_head(&mut filesystem, Some(inode.number()), 764);
    drop(filesystem);

    let mount_device: Arc<block::BlockDevice> = crate::io::test_block_device(device.clone());
    assert_eq!(
        Ext4SbInfo::mount(mount_device).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );
}

#[def_test]
fn recover_rejects_clean_legacy_orphan_without_a_journal() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    persist_test_orphan_head(&mut filesystem, Some(inode.number()), 765);
    drop(filesystem);

    let recovery_device: Arc<block::BlockDevice> = crate::io::test_block_device(device.clone());
    assert_eq!(
        Ext4SbInfo::recover(recovery_device),
        Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))
    );
    let bytes = device.bytes();
    let superblock = Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
        .expect("decode persisted clean orphan head");
    assert_eq!(superblock.last_orphan(), inode.number().get());
}

#[def_test]
fn recover_rejects_orphan_file_without_backing_inode() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::ORPHAN_FILE,
        features::ReadOnlyCompatFeatures::empty(),
    );
    drop(filesystem);

    let recovery_device: Arc<block::BlockDevice> = crate::io::test_block_device(device.clone());
    assert_eq!(
        Ext4SbInfo::recover(recovery_device),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))
    );
}

#[def_test]
fn journal_location_rejects_ambiguous_or_missing_journal_fields() {
    let inode = NonZeroU32::new(8);
    let device = NonZeroU32::new(1);

    assert_eq!(
        select_journal_location(false, inode, None, [0; 16]),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
    assert_eq!(
        select_journal_location(true, inode, device, [0; 16]),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
    assert_eq!(
        select_journal_location(true, None, None, [0; 16]),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn journal_location_preserves_external_device_and_uuid() {
    let device = NonZeroU32::new(7).unwrap();
    let uuid = [0x5a; 16];

    assert_eq!(
        select_journal_location(true, None, Some(device), uuid),
        Ok(JournalLocation::External { dev: device, uuid })
    );
}

#[def_test]
fn journal_mapping_requires_every_block_to_be_written() {
    for invalid in [
        BlockMapping::Hole {
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        },
        BlockMapping::Unwritten {
            physical: PhysicalBlock::new(100),
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        },
        BlockMapping::Mapped {
            physical: PhysicalBlock::new(100),
            len: BlockCount::new(0),
            flags: crate::BlockMappingFlags::empty(),
        },
    ] {
        assert_eq!(
            collect_journal_extents(1, |_| Ok(invalid)),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
        );
    }
}

#[def_test]
fn journal_mapping_records_complete_mapped_ranges() {
    let extents = collect_journal_extents(4, |logical| match logical {
        0 => Ok(BlockMapping::Mapped {
            physical: PhysicalBlock::new(100),
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        }),
        2 => Ok(BlockMapping::Mapped {
            physical: PhysicalBlock::new(200),
            len: BlockCount::new(4),
            flags: crate::BlockMappingFlags::empty(),
        }),
        _ => unreachable!(),
    })
    .unwrap();

    assert_eq!(
        extents,
        vec![
            JournalExtent {
                logical_start: 0,
                physical_start: 100,
                len: 2,
            },
            JournalExtent {
                logical_start: 2,
                physical_start: 200,
                len: 2,
            },
        ]
    );
}

#[def_test]
fn mounted_journal_rejects_extent_beyond_filesystem_device() {
    let storage = InternalJournal {
        superblock: test_journal_superblock(900),
        extents: vec![JournalExtent {
            logical_start: 0,
            physical_start: 16,
            len: 1024,
        }],
        block_count: 1024,
    };

    assert!(matches!(
        MountedJournal::new(storage, 32),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    ));
}

#[def_test]
fn mounted_journal_accepts_inode_capacity_larger_than_s_maxlen() {
    let storage = InternalJournal {
        superblock: test_journal_superblock(900),
        extents: vec![JournalExtent {
            logical_start: 0,
            physical_start: 16,
            len: 1025,
        }],
        block_count: 1024,
    };

    assert!(MountedJournal::new(storage, 2048).is_ok());
}

#[def_test]
fn mounted_journal_rejects_mapping_shorter_than_s_maxlen() {
    let storage = InternalJournal {
        superblock: test_journal_superblock(900),
        extents: vec![JournalExtent {
            logical_start: 0,
            physical_start: 16,
            len: 1023,
        }],
        block_count: 1024,
    };

    assert!(matches!(
        MountedJournal::new(storage, 2048),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    ));
}

#[def_test]
fn physical_block_validation_rejects_filesystem_metadata_zones() {
    let zones = [
        SystemZone {
            start: 20,
            end: 30,
            owner: None,
        },
        SystemZone {
            start: 40,
            end: 45,
            owner: Some(InodeNumber::new(8)),
        },
        SystemZone {
            start: 45,
            end: 50,
            owner: None,
        },
    ];

    assert!(!is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(12),
        20,
        1
    ));
    assert!(is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(12),
        30,
        1
    ));
    assert!(is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(8),
        40,
        5
    ));
    assert!(!is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(8),
        40,
        10
    ));
    assert!(!is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(12),
        0,
        1
    ));
    assert!(!is_inode_physical_block_valid(
        0,
        100,
        &zones,
        InodeNumber::new(12),
        99,
        2
    ));
}

#[def_test]
fn orlov_top_level_directory_uses_child_name_hash_start() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    }; 4];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    let child_name = b"hashed-top-level-dir";
    let expected_flex = filesystem.orlov_top_level_start_flex(Some(child_name));
    let journal = JournalTransactions::new(TransactionId::new(826));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

    let allocation = filesystem
        .allocate_named_inode(
            Some(InodeNumber::new(2)),
            child_name,
            InodeInitialization::directory(0o755, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(expected_flex));
}

#[def_test]
fn orlov_hash_ignores_htree_default_version_and_unsigned_policy() {
    let name = [0x80];
    for default_hash_version in [
        crate::disk::dir::DX_HASH_LEGACY,
        crate::disk::dir::DX_HASH_HALF_MD4,
        crate::disk::dir::DX_HASH_TEA,
    ] {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        set_allocator_default_hash_version(&mut filesystem, default_hash_version);
        assert_eq!(
            filesystem.superblock().default_hash_version(),
            default_hash_version
        );
        filesystem.hash_unsigned = 0;
        let signed_half_md4 = filesystem
            .htree_hash(&name, crate::disk::dir::DX_HASH_HALF_MD4)
            .unwrap();
        let selected_default = filesystem.htree_hash(&name, default_hash_version).unwrap();
        if default_hash_version != crate::disk::dir::DX_HASH_HALF_MD4 {
            assert_ne!(selected_default, signed_half_md4);
        }

        filesystem.hash_unsigned = DX_HASH_UNSIGNED_OFFSET;
        let unsigned_half_md4 = filesystem
            .htree_hash(&name, crate::disk::dir::DX_HASH_HALF_MD4)
            .unwrap();

        assert_ne!(unsigned_half_md4, signed_half_md4);
        assert_eq!(
            filesystem.orlov_child_name_hash(Some(&name)),
            signed_half_md4.major()
        );
    }
}

#[def_test]
fn mount_validates_checksums_and_device_geometry() {
    use super::test_fixtures::{allocator_test_filesystem_with_options, open_test_filesystem};
    let (filesystem, device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        false,
        false,
    );
    drop(filesystem);
    let valid = device.bytes();
    let mounted = open_test_filesystem(crate::io::test_block_device(device))
        .expect("valid checksummed metadata mounts");
    assert_eq!(
        mounted.groups().len(),
        mounted.layout().group_count() as usize
    );
    assert!(mounted.journal.is_none());
    for offset in [1024 + 0x10, TEST_BLOCK_SIZE + 12] {
        let mut corrupt = valid.clone();
        corrupt[offset] ^= 1;
        assert!(matches!(
            open_test_filesystem(crate::io::test_block_device(Arc::new(TestDevice::new(
                corrupt
            )))),
            Err(Ext4Error::ChecksumMismatch { .. })
        ));
    }
    let mut short = valid;
    short.truncate(short.len() / 2);
    assert!(matches!(
        open_test_filesystem(crate::io::test_block_device(Arc::new(TestDevice::new(
            short
        )))),
        Err(Ext4Error::OutOfBounds)
    ));
}

#[def_test]
fn public_inode_load_rejects_reserved_internal_journal() {
    let (filesystem, _) = super::test_fixtures::recovery_test_filesystem();
    assert!(matches!(
        filesystem.load_inode_private(InodeNumber::new(8)),
        Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode))
    ));
}
