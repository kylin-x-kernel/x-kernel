// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Quota metadata I/O through canonical VFS inodes and the mounted journal.

use alloc::{sync::Arc, vec};

use kerrno::{KError, LinuxError};
use kquota::{ChargeFlags, QuotaFormat, QuotaId, QuotaIo, QuotaState, QuotaType};
use unittest::def_test;

use super::*;
use crate::{
    FilesystemBlock,
    jbd2::JournalCredits,
    quota::Ext4QuotaIo,
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, TestDevice, allocate_checkpointed_regular_inode,
        install_test_internal_journal, journal_allocator_test_filesystem,
    },
};

fn quota_fixture() -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<TestDevice>) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 9501);
    let (super_block, file) = super::read_tests::cached_file(
        filesystem,
        inode,
        SuperBlockFlags::empty(),
        kvfs::OpenFlags::empty(),
    );
    (super_block, file, device)
}

fn load_user_quota(super_block: &SuperBlock, inode: &Arc<VfsInode>) {
    let mut state = ext4_private(super_block).unwrap().write(super_block);
    let credits = JournalCredits::new(256);
    let required_blocks = 2 + state.inode_extra_isize_allocation_blocks(inode.private().unwrap());
    let journal = state
        .metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            required_blocks,
        )
        .unwrap();
    let mut handle = journal.begin(credits).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    let mut header = [0; 2048];
    header[..4].copy_from_slice(&0xd9c0_1f11_u32.to_le_bytes());
    header[4..8].copy_from_slice(&1_u32.to_le_bytes());
    header[20..24].copy_from_slice(&2_u32.to_le_bytes());
    io.write(inode, 0, &header).unwrap();
    super_block
        .quota_info()
        .enable(
            &mut io,
            QuotaType::User,
            inode,
            QuotaFormat::VfsV1,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
}

#[def_test]
fn quota_payload_is_visible_from_metadata_cache_before_checkpoint() {
    let (super_block, file, device) = quota_fixture();
    let inode = file.inode().clone();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let credits = JournalCredits::new(256);
    let journal = state
        .metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            2,
        )
        .unwrap();
    let mut handle = journal.begin(credits).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    assert_eq!(io.write(&inode, 19, b"quota-data"), Ok(10));
    let mut output = [0xff; 40];
    assert_eq!(io.read(&inode, 0, &mut output), Ok(29));
    assert_eq!(&output[..19], &[0; 19]);
    assert_eq!(&output[19..29], b"quota-data");
    assert_eq!(&output[29..], &[0xff; 11]);
    let BlockMapping::Mapped { physical, .. } = state
        .map_blocks(inode.private().unwrap(), LogicalBlock::new(0))
        .unwrap()
    else {
        panic!("missing quota block")
    };
    let image = device.bytes();
    let start = physical.get() as usize * TEST_BLOCK_SIZE;
    assert_ne!(&image[start + 19..start + 29], b"quota-data");
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
    let image = device.bytes();
    assert_eq!(&image[start + 19..start + 29], b"quota-data");
    assert_eq!(
        state
            .load_inode_private(inode.private::<Ext4Inode>().unwrap().number())
            .unwrap()
            .size(),
        29
    );
}

#[def_test]
fn quota_write_rejects_cross_block_requests_before_mutating_metadata() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    assert_eq!(
        io.write(&inode, TEST_BLOCK_SIZE as u64 - 1, &[1, 2]),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(io.file_len(&inode), Ok(0));
    assert!(!handle.has_updates());
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn quota_reads_cross_blocks_and_zero_sparse_holes() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    io.write(&inode, 2 * TEST_BLOCK_SIZE as u64 + 7, b"tail")
        .unwrap();
    let mut output = vec![0xff; 2 * TEST_BLOCK_SIZE + 16];
    assert_eq!(
        io.read(&inode, 0, &mut output),
        Ok(2 * TEST_BLOCK_SIZE + 11)
    );
    assert!(
        output[..2 * TEST_BLOCK_SIZE + 7]
            .iter()
            .all(|&byte| byte == 0)
    );
    assert_eq!(
        &output[2 * TEST_BLOCK_SIZE + 7..2 * TEST_BLOCK_SIZE + 11],
        b"tail"
    );
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn filesystem_sync_persists_quota_records_using_the_retained_inode() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let owner = QuotaId::new(QuotaType::User, 42).unwrap();
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let mut io = Ext4QuotaIo {
            filesystem: &mut state,
            handle: &mut handle,
        };
        let mut header = [0; 1024];
        header[..4].copy_from_slice(&0xd9c0_1f11_u32.to_le_bytes());
        header[4..8].copy_from_slice(&1_u32.to_le_bytes());
        header[20..24].copy_from_slice(&2_u32.to_le_bytes());
        io.write(&inode, 0, &header).unwrap();
        io.write(&inode, 1024, &[0; 1024]).unwrap();
        let mut quota = super_block.quota_info();
        quota
            .enable(
                &mut io,
                QuotaType::User,
                &inode,
                QuotaFormat::VfsV1,
                QuotaState::USAGE_ENABLED,
            )
            .unwrap();
        quota
            .alloc_space(&mut io, &[owner], 8192, 0, ChargeFlags::WARN)
            .unwrap();
        quota.alloc_inode(&mut io, &[owner], 0, false).unwrap();
        assert_eq!(quota.next_dirty_id(None), Some(owner));
        assert!(Arc::ptr_eq(
            quota.info(QuotaType::User).unwrap().file(),
            &inode
        ));
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
    }
    super_block.sync_fs().unwrap();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    let mut reopened = kquota::QuotaInfo::new();
    reopened
        .enable(
            &mut io,
            QuotaType::User,
            &inode,
            QuotaFormat::VfsV1,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    let data = reopened.get(&mut io, owner).unwrap();
    assert_eq!(data.used_bytes, 8192);
    assert_eq!(data.used_inodes, 1);
    assert_eq!(super_block.quota_info().next_dirty_id(None), None);
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn quota_write_initializes_unwritten_blocks_without_exposing_old_contents() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let private = inode.private::<Ext4Inode>().unwrap();
    let allocation = state.allocate_block(private, None, &mut handle).unwrap();
    state
        .write_contiguous_blocks(
            FilesystemBlock::new(allocation.block().get()),
            1,
            &vec![0x5a; TEST_BLOCK_SIZE],
        )
        .unwrap();
    state
        .insert_extent_mapping(
            private,
            LogicalBlock::new(0),
            allocation.block(),
            crate::BlockCount::new(1),
            crate::extent::ExtentMappingState::Unwritten,
            &mut handle,
        )
        .unwrap();
    state
        .update_regular_inode_write_metadata(
            private,
            TEST_BLOCK_SIZE as u64,
            crate::file::RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    private.set_size(TEST_BLOCK_SIZE as u64);
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    io.write(&inode, 7, b"quota").unwrap();
    let mut output = vec![0xff; TEST_BLOCK_SIZE];
    assert_eq!(io.read(&inode, 0, &mut output), Ok(TEST_BLOCK_SIZE));
    assert_eq!(&output[7..12], b"quota");
    assert!(
        output[..7]
            .iter()
            .chain(output[12..].iter())
            .all(|&byte| byte == 0)
    );
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn quota_write_checks_credit_bound_before_allocating_a_hole() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let free_blocks = state.free_blocks_count();
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    assert_eq!(
        io.write(&inode, 0, b"quota"),
        Err(KError::from(LinuxError::ENOSPC).canonicalize())
    );
    assert_eq!(io.file_len(&inode), Ok(0));
    assert!(!handle.has_updates());
    assert_eq!(state.free_blocks_count(), free_blocks);
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn sync_reports_failed_acquisition_without_aborting_or_initializing_the_record() {
    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let owner = QuotaId::new(QuotaType::User, 43).unwrap();
    load_user_quota(&super_block, &inode);
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let result = super_block.quota_info().get(
            &mut Ext4QuotaIo {
                filesystem: &mut state,
                handle: &mut handle,
            },
            owner,
        );
        assert_eq!(result, Err(KError::from(LinuxError::ENOSPC).canonicalize()));
        assert!(!handle.has_updates());
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
        assert!(!journal.is_aborted());
    }
    assert_eq!(super_block.sync_fs(), Err(KError::from(LinuxError::EIO)));
    assert!(!super_block.flags().contains(SuperBlockFlags::RDONLY));
    assert_eq!(super_block.quota_info().next_dirty_id(None), Some(owner));
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let credits = JournalCredits::new(256);
        let journal = state
            .metadata_journal_for_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                2,
            )
            .unwrap();
        let mut handle = journal.begin(credits).unwrap();
        let mut io = Ext4QuotaIo {
            filesystem: &mut state,
            handle: &mut handle,
        };
        let mut quota = super_block.quota_info();
        quota.get(&mut io, owner).unwrap();
        quota.alloc_inode(&mut io, &[owner], 0, false).unwrap();
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
    }
    super_block.sync_fs().unwrap();
    assert_eq!(super_block.quota_info().next_dirty_id(None), None);
}

#[def_test]
fn quota_growth_admits_external_xattr_space_for_extra_isize_expansion() {
    use crate::{
        Ext4Timestamp, Ext4XattrNamespace, superblock::test_fixtures::update_test_inode_extra_isize,
    };

    let (super_block, file, _) = quota_fixture();
    let inode = file.inode().clone();
    let private = inode.private::<Ext4Inode>().unwrap();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    update_test_inode_extra_isize(&mut state, private, 0, &mut handle);
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    let value = vec![0x71; 88];
    state
        .set_xattr(
            private,
            Ext4XattrNamespace::User,
            b"key",
            &value,
            Ext4Timestamp::new(1, 0),
        )
        .unwrap();
    assert_eq!(private.extra_isize(), 0);
    state.sync_filesystem().unwrap();
    let extra_blocks = state.inode_extra_isize_allocation_blocks(private);
    assert_eq!(extra_blocks, 1);
    let required_blocks = 1
        + state
            .extent_insert_metadata_block_bound(
                private,
                LogicalBlock::new(0),
                crate::BlockCount::new(1),
            )
            .unwrap()
        + extra_blocks;
    let credits = JournalCredits::new(256);
    let journal = state
        .metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            required_blocks,
        )
        .unwrap();
    let free_blocks = state.free_blocks_count();
    let mut handle = journal.begin(credits).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &mut state,
        handle: &mut handle,
    };
    io.write(&inode, 0, b"quota").unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
    assert_eq!(private.extra_isize(), state.superblock().want_extra_isize());
    assert_eq!(
        state
            .get_xattr(private, Ext4XattrNamespace::User, b"key")
            .unwrap(),
        Some(value)
    );
    assert_eq!(free_blocks - state.free_blocks_count(), required_blocks);
    assert_eq!(private.size(), 5);
}

#[def_test]
fn quota_feature_mount_rejection_precedes_storage_writes() {
    use crate::{Ext4Error, FeatureClass, disk::ReadOnlyCompatFeatures};

    for flags in [
        ReadOnlyCompatFeatures::QUOTA,
        ReadOnlyCompatFeatures::PROJECT,
    ] {
        let (_, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let mut image = device.bytes();
        let offset = 1024 + 0x64;
        image[offset..offset + 4].copy_from_slice(&flags.bits().to_le_bytes());
        let device = Arc::new(TestDevice::new(image));
        let result = Ext4SbInfo::open(crate::io::test_block_device(device.clone()), true);
        assert!(matches!(result, Err(Ext4Error::UnsupportedFeature {
            class: FeatureClass::ReadOnlyCompatible, bits,
        }) if bits == flags.bits()));
        assert_eq!(device.write_count(), 0);
    }
}
