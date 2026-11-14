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
    quota_fixture_at(None)
}

fn quota_fixture_at(
    quota_number: Option<InodeNumber>,
) -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<TestDevice>) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 9501);
    let inode = if let Some(quota_number) = quota_number {
        let block = filesystem.inode_table_entry_block(inode.number()).unwrap();
        assert_eq!(
            filesystem.inode_table_entry_block(quota_number).unwrap(),
            block
        );
        let inode_size = usize::from(filesystem.superblock().inode_size());
        let source = (inode.number().get() as usize - 1) * inode_size % TEST_BLOCK_SIZE;
        let target = (quota_number.get() as usize - 1) * inode_size % TEST_BLOCK_SIZE;
        let mut bytes = filesystem
            .read_metadata_block(block)
            .unwrap()
            .as_ref()
            .to_vec();
        bytes.copy_within(source..source + inode_size, target);
        let journal = filesystem.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let access = filesystem
            .metadata_io
            .write_access(block, &mut handle)
            .unwrap();
        crate::superblock::replace_metadata_access_bytes(&access, bytes).unwrap();
        filesystem
            .complete_metadata_mutation(handle, Ok(()))
            .unwrap();
        filesystem.sync_filesystem().unwrap();
        filesystem.special_iget(quota_number).unwrap()
    } else {
        inode
    };
    let (super_block, file) = super::read_tests::cached_file(
        filesystem,
        inode,
        SuperBlockFlags::empty(),
        kvfs::OpenFlags::empty(),
    );
    (super_block, file, device)
}

fn load_user_quota(super_block: &SuperBlock, inode: &Arc<VfsInode>) {
    load_quota(super_block, inode, QuotaType::User);
}

fn load_quota(super_block: &SuperBlock, inode: &Arc<VfsInode>, kind: QuotaType) {
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
        handle: Some(&mut handle),
    };
    let mut header = [0; 2048];
    let magic: u32 = match kind {
        QuotaType::User => 0xd9c0_1f11,
        QuotaType::Group => 0xd9c0_1927,
        QuotaType::Project => 0xd9c0_3f14,
    };
    header[..4].copy_from_slice(&magic.to_le_bytes());
    header[4..8].copy_from_slice(&1_u32.to_le_bytes());
    header[20..24].copy_from_slice(&2_u32.to_le_bytes());
    io.write(inode, 0, &header).unwrap();
    super_block
        .quota_info()
        .enable(
            &mut io,
            kind,
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
        handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
            handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
                handle: Some(&mut handle),
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
    assert_eq!(super_block.quota_info().next_dirty_id(None), None);
    super_block.sync_fs().unwrap();
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
            handle: Some(&mut handle),
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
        handle: Some(&mut handle),
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
fn project_feature_allows_writable_mount() {
    use crate::disk::ReadOnlyCompatFeatures;

    {
        let flags = ReadOnlyCompatFeatures::PROJECT;
        let (_, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let mut image = device.bytes();
        let offset = 1024 + 0x64;
        image[offset..offset + 4].copy_from_slice(&flags.bits().to_le_bytes());
        let device = Arc::new(TestDevice::new(image));
        let result = Ext4SbInfo::mount(crate::io::test_block_device(device.clone()));
        assert!(result.is_ok());
    }
}

#[def_test]
fn quota_feature_read_only_preparation_preserves_storage() {
    use crate::disk::ReadOnlyCompatFeatures;
    for flags in [
        ReadOnlyCompatFeatures::QUOTA,
        ReadOnlyCompatFeatures::PROJECT,
    ] {
        let (_, original) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let mut image = original.bytes();
        let offset = 1024 + 0x64;
        image[offset..offset + 4].copy_from_slice(&flags.bits().to_le_bytes());
        let device = Arc::new(TestDevice::new(image.clone()));
        let mut state =
            Ext4SbInfo::open(crate::io::test_block_device(device.clone()), true).unwrap();
        let replay = state.prepare_recovery().unwrap();
        state.finish_recovery(replay).unwrap();
        assert!(state.ensure_writable().is_ok());
        assert_eq!(device.write_count(), 0);
        assert_eq!(device.bytes(), image);
    }
}

fn internal_quota_fixture() -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<TestDevice>) {
    let (super_block, file, device) = quota_fixture_at(Some(InodeNumber::new(
        crate::disk::inode::EXT4_USR_QUOTA_INO,
    )));
    load_user_quota(&super_block, file.inode());
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        state.vfs_superblock = Arc::downgrade(&super_block);
        let (block, offset, length) = state.primary_superblock_location().unwrap();
        let mut bytes = state.read_metadata_block(block).unwrap().as_ref().to_vec();
        let flags = u32::from_le_bytes(bytes[offset + 0x64..offset + 0x68].try_into().unwrap());
        bytes[offset + 0x64..offset + 0x68].copy_from_slice(
            &(flags | crate::disk::ReadOnlyCompatFeatures::QUOTA.bits()).to_le_bytes(),
        );
        bytes[offset + 0x240..offset + 0x244]
            .copy_from_slice(&(file.inode().inode() as u32).to_le_bytes());
        let decoded =
            crate::disk::Ext4DiskSuperblock::decode(&bytes[offset..offset + length]).unwrap();
        let journal = state
            .metadata_journal_without_allocation(
                JournalCredits::new(1),
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .unwrap();
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let access = state.metadata_io.write_access(block, &mut handle).unwrap();
        crate::superblock::replace_metadata_access_bytes(&access, bytes).unwrap();
        state.superblock = decoded;
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
        state.sync_filesystem().unwrap();
    }
    (super_block, file, device)
}

fn allocate_accounted_inode(state: &mut Ext4SbInfo, uid: u32) -> Arc<Ext4Inode> {
    state.initialize_owner_quotas(uid, uid, 0).unwrap();
    let credits = state.quota_metadata_credits(JournalCredits::new(128));
    let journal = state
        .metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            1,
        )
        .unwrap();
    let mut handle = journal.begin(credits).unwrap();
    let allocation = state
        .allocate_inode(
            None,
            crate::inode::InodeInitialization::regular_file(0o600, uid, uid),
            &mut handle,
        )
        .unwrap();
    let inode = state.internal_iget(allocation.inode()).unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.bind_inode_quotas(&inode).unwrap();
    Arc::new(inode)
}

fn owner_usage(super_block: &SuperBlock, state: &mut Ext4SbInfo, uid: u32) -> kquota::Dqblk {
    quota_usage(super_block, state, QuotaType::User, uid)
}

fn quota_usage(super_block: &SuperBlock, state: &mut Ext4SbInfo, kind: QuotaType, id: u32) -> kquota::Dqblk {
    let journal = state.journal.as_ref().unwrap().clone();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let data = super_block
        .quota_info()
        .get(
            &mut Ext4QuotaIo {
                filesystem: state,
                handle: Some(&mut handle),
            },
            QuotaId::new(kind, id).unwrap(),
        )
        .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    data
}

#[def_test]
fn immutable_project_attributes_follow_the_project_feature() {
    use crate::disk::{
        CompatFeatures, ReadOnlyCompatFeatures,
        inode::{EXT4_APPEND_FL, EXT4_CASEFOLD_FL, EXT4_IMMUTABLE_FL},
    };
    use crate::superblock::test_fixtures::set_allocator_feature_bits;

    let (super_block, file, _) = quota_fixture();
    let vfs_inode = file.inode();
    let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();
    let now = current_ext4_timestamp(vfs_inode);
    let initial_ctime = Ext4Timestamp::new(now.seconds().saturating_sub(3600), now.nanos());
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        assert!(!state.superblock().features().has_project());
        state
            .set_inode_user_flags(
                &inode,
                inode.flags() | EXT4_IMMUTABLE_FL,
                initial_ctime,
            )
            .unwrap();
    }
    let attributes = kvfs::FileAttr {
        flags: inode.flags(),
        project_id: 42,
        flags_valid: false,
    };
    let cred = kcred::initial_cred();
    assert_eq!(
        file.path().fileattr_set(attributes, &cred),
        Err(VfsError::OperationNotSupported)
    );
    assert_ne!(inode.stat().ctime, initial_ctime);
    assert_eq!(inode.project_id(), 0);

    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        state.sync_filesystem().unwrap();
        set_allocator_feature_bits(
            &mut state,
            CompatFeatures::empty(),
            ReadOnlyCompatFeatures::PROJECT,
        );
        state.metadata_io.invalidate_all();
    }
    assert_eq!(
        file.path().fileattr_set(attributes, &cred),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        file.path().fileattr_set(
            kvfs::FileAttr {
                flags: attributes.flags | EXT4_APPEND_FL,
                project_id: 0,
                ..attributes
            },
            &cred,
        ),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        file.path().fileattr_set(
            kvfs::FileAttr {
                flags: attributes.flags | EXT4_CASEFOLD_FL,
                flags_valid: true,
                ..attributes
            },
            &cred,
        ),
        Err(VfsError::OperationNotSupported)
    );
}

#[def_test]
fn project_change_transfers_inode_and_delalloc_reservation() {
    use crate::disk::{CompatFeatures, ReadOnlyCompatFeatures};
    use crate::superblock::test_fixtures::set_allocator_feature_bits;

    let (super_block, file, _) = quota_fixture();
    load_quota(&super_block, file.inode(), QuotaType::Project);
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    state.sync_filesystem().unwrap();
    set_allocator_feature_bits(&mut state, CompatFeatures::empty(), ReadOnlyCompatFeatures::PROJECT);
    state.metadata_io.invalidate_all();
    state.vfs_superblock = Arc::downgrade(&super_block);
    state.quota_mount_options = 4;
    file.inode().private::<Ext4Inode>().unwrap().set_noquota(true);

    let inode = allocate_accounted_inode(&mut state, 1000);
    let block_size = i64::from(state.layout().block_size());
    state.reserve_delalloc_range_into(&inode, LogicalBlock::new(0), 1).unwrap();
    let before = quota_usage(&super_block, &mut state, QuotaType::Project, 0);
    assert_eq!((before.used_inodes, before.reserved_bytes), (1, block_size));

    state.set_inode_project_id(&inode, 42, Ext4Timestamp::new(1, 0)).unwrap();
    assert_eq!(inode.project_id(), 42);
    assert_eq!(state.raw_inode(inode.number()).unwrap().project_id(), Some(42));
    assert_eq!(state.internal_iget(inode.number()).unwrap().project_id(), 42);
    let old = quota_usage(&super_block, &mut state, QuotaType::Project, 0);
    let new = quota_usage(&super_block, &mut state, QuotaType::Project, 42);
    assert_eq!((old.used_inodes, old.reserved_bytes), (0, 0));
    assert_eq!((new.used_inodes, new.reserved_bytes), (1, block_size));

    state.release_all_delalloc(&inode).unwrap();
    assert_eq!(quota_usage(&super_block, &mut state, QuotaType::Project, 42).reserved_bytes, 0);
    inode.drop_quota_records(None);
}

#[def_test]
fn internal_quota_uses_canonical_inode_and_hides_it_from_namespace_lookup() {
    let (super_block, file, _device) = internal_quota_fixture();
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
    Ext4SuperOperations::activate_internal_quotas(&super_block).unwrap();
    assert!(Arc::ptr_eq(
        super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .file(),
        file.inode()
    ));
    assert!(matches!(
        Ext4SuperOperations::iget(&super_block, InodeNumber::new(file.inode().inode() as u32)),
        Err(VfsError::OperationNotSupported)
    ));
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
    assert!(super_block.quota_info().info(QuotaType::User).is_none());
}

#[def_test]
fn internal_quota_claims_delalloc_once_and_chown_transfers_usage_and_reservations() {
    let (super_block, _file, _device) = internal_quota_fixture();
    let vfs_inode = accounted_vfs_inode(&super_block, 1000);
    let path = inode_path(&super_block, &vfs_inode);
    let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let block_size = u64::from(state.layout().block_size());
    state
        .reserve_delalloc_range_into(&inode, LogicalBlock::new(0), 2)
        .unwrap();
    let initial = owner_usage(&super_block, &mut state, 1000);
    assert_eq!(
        (
            initial.used_inodes,
            initial.used_bytes,
            initial.reserved_bytes
        ),
        (1, 0, 2 * block_size as i64)
    );
    assert_eq!(path.getattr().unwrap().blocks * 512, 2 * block_size);
    inode.set_size(block_size);
    drop(state);
    let quota = super_block.quota_info();
    let writer = {
        let super_block = super_block.clone();
        let inode = inode.clone();
        ktask::spawn(move || {
            ext4_private(&super_block)
                .unwrap()
                .write(&super_block)
                .writeback_ordered_at_with_prealloc_budget(
                    &inode,
                    0,
                    &vec![0x5a; block_size as usize],
                    block_size,
                    crate::Ext4SyncIntent::FullMetadata,
                    0,
                    || true,
                )
                .unwrap();
        })
    };
    while !writer.state().is_blocked() {
        assert_ne!(writer.state(), ktask::TaskState::Exited);
        ktask::yield_now();
    }
    let observed = Arc::new(ksync::Mutex::new(None));
    let reader = {
        let path = path.clone();
        let observed = observed.clone();
        ktask::spawn(move || *observed.lock() = Some(path.getattr().unwrap().blocks))
    };
    while !reader.state().is_blocked() {
        assert_ne!(reader.state(), ktask::TaskState::Exited);
        ktask::yield_now();
    }
    drop(quota);
    writer.join();
    reader.join();
    assert_eq!(*observed.lock(), Some(2 * block_size / 512));
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let allocated = owner_usage(&super_block, &mut state, 1000);
    assert_eq!(
        (allocated.used_bytes, allocated.reserved_bytes),
        (block_size as i64, block_size as i64)
    );
    assert_eq!(path.getattr().unwrap().blocks * 512, 2 * block_size);
    assert_eq!(state.delalloc_reserved_blocks(), 1);
    state
        .update_inode_metadata(
            &inode,
            crate::Ext4InodeMetadataUpdate::new(crate::Ext4Timestamp::new(1, 0))
                .with_owner(2000, 2000),
        )
        .unwrap();
    let old = owner_usage(&super_block, &mut state, 1000);
    let new = owner_usage(&super_block, &mut state, 2000);
    assert_eq!(
        (old.used_inodes, old.used_bytes, old.reserved_bytes),
        (0, 0, 0)
    );
    assert_eq!(
        (new.used_inodes, new.used_bytes, new.reserved_bytes),
        (1, block_size as i64, block_size as i64)
    );
    state.release_all_delalloc(&inode).unwrap();
    state
        .truncate_regular_inode(&inode, 0, crate::Ext4Timestamp::new(2, 0))
        .unwrap();
    let new = owner_usage(&super_block, &mut state, 2000);
    assert_eq!((new.used_bytes, new.reserved_bytes), (0, 0));
    assert_eq!(path.getattr().unwrap().blocks, 0);
}

#[def_test]
fn quota_limits_reject_inode_and_space_before_bitmap_publication() {
    use crate::{
        BlockCount,
        inode::InodeInitialization,
        mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    };
    let (super_block, _file, _device) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    state.initialize_owner_quotas(1000, 1000, 0).unwrap();
    let block_size = u64::from(state.layout().block_size());
    let id = QuotaId::new(QuotaType::User, 1000).unwrap();
    let journal = state
        .metadata_journal_without_allocation(
            JournalCredits::new(1),
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    {
        let mut quota = super_block.quota_info();
        let mut io = Ext4QuotaIo {
            filesystem: &state,
            handle: Some(&mut handle),
        };
        quota
            .set(
                &mut io,
                id,
                kquota::Dqblk {
                    inode_hard_limit: 1,
                    space_hard_limit_bytes: block_size as i64,
                    ..kquota::Dqblk::default()
                },
                kquota::QuotaFields::INODE_HARD | kquota::QuotaFields::SPACE_HARD,
                1,
            )
            .unwrap();
        quota.set_limits_enabled(QuotaType::User, true).unwrap();
        quota.sync_dquot(&mut io, id).unwrap();
    }
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    let inode = allocate_accounted_inode(&mut state, 1000);
    let free_inodes = state.free_inodes_count();
    let free_blocks = state.free_blocks_count();
    let journal = state
        .metadata_journal_without_allocation(
            JournalCredits::new(128),
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .unwrap();
    let mut handle = journal.begin(JournalCredits::new(128)).unwrap();
    assert_eq!(
        state.allocate_inode(
            None,
            InodeInitialization::regular_file(0o600, 1000, 1000),
            &mut handle
        ),
        Err(Ext4Error::Quota(LinuxError::EDQUOT))
    );
    let exact = Ext4AllocationRequest::new(
        &inode,
        LogicalBlock::new(0),
        None,
        BlockCount::new(2),
        BlockCount::new(2),
        Ext4AllocationFlags::EXACT,
    )
    .unwrap();
    assert_eq!(
        state.allocate_blocks_for_write(exact, &mut handle),
        Err(Ext4Error::Quota(LinuxError::EDQUOT))
    );
    assert_eq!(inode.blocks(), 0);
    assert!(!handle.has_updates());
    assert_eq!(
        (state.free_inodes_count(), state.free_blocks_count()),
        (free_inodes, free_blocks)
    );
    let partial = Ext4AllocationRequest::new(
        &inode,
        LogicalBlock::new(0),
        None,
        BlockCount::new(2),
        BlockCount::new(1),
        Ext4AllocationFlags::ALLOW_PARTIAL,
    )
    .unwrap();
    let allocated = state
        .allocate_blocks_for_write(partial, &mut handle)
        .unwrap();
    assert_eq!(allocated.block_count().get(), 1);
    let used_bytes = super_block
        .quota_info()
        .get(
            &mut Ext4QuotaIo {
                filesystem: &state,
                handle: Some(&mut handle),
            },
            id,
        )
        .unwrap()
        .used_bytes;
    assert_eq!(inode.blocks() * 512, block_size);
    assert_eq!(used_bytes, block_size as i64);
    state
        .release_allocated_block(
            &inode,
            allocated.first_block_allocation().block(),
            &mut handle,
        )
        .unwrap();
    assert_eq!(inode.blocks(), 0);
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    assert!(!state.journal.as_ref().unwrap().is_aborted());
    let usage = owner_usage(&super_block, &mut state, 1000);
    assert_eq!((usage.used_inodes, usage.used_bytes), (1, 0));
}

fn remount_quota_filesystem(super_block: &SuperBlock, is_read_only: bool) -> VfsResult<()> {
    let cred = kcred::initial_cred();
    let flags = if is_read_only {
        SuperBlockFlags::RDONLY
    } else {
        SuperBlockFlags::empty()
    };
    let mut context = kvfs::FsContext::new_reconfigure(
        super_block,
        None,
        None,
        flags,
        SuperBlockFlags::RDONLY,
        &cred,
    )?;
    super_block.reconfigure(&mut context)
}

fn accounted_vfs_inode(super_block: &Arc<SuperBlock>, uid: u32) -> Arc<VfsInode> {
    let inode = {
        let mut state = ext4_private(super_block).unwrap().write(super_block);
        allocate_accounted_inode(&mut state, uid)
    };
    Ext4SuperOperations::iget_from_private(super_block, Arc::try_unwrap(inode).ok().unwrap())
        .unwrap()
}

fn inode_path(super_block: &Arc<SuperBlock>, inode: &Arc<VfsInode>) -> kvfs::Path {
    kvfs::Path::new(
        kvfs::Mount::new_root(super_block),
        Dentry::new_file_from_inode(inode.clone(), Some(super_block.root_dir()), String::new()),
    )
}

#[def_test]
fn mapped_quota_overwrites_reuse_a_consumed_transaction_credit() {
    let (super_block, file, _) = quota_fixture();
    load_user_quota(&super_block, file.inode());
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state
        .metadata_journal_without_allocation(
            JournalCredits::new(1),
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &state,
        handle: Some(&mut handle),
    };
    io.write(file.inode(), 80, b"one").unwrap();
    assert_eq!(io.handle.as_ref().unwrap().remaining_credits(), 0);
    io.write(file.inode(), 80, b"two").unwrap();
    let mut bytes = [0; 3];
    assert_eq!(io.read(file.inode(), 80, &mut bytes), Ok(3));
    assert_eq!(&bytes, b"two");
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn a_new_quota_payload_target_still_requires_a_transaction_credit() {
    let (super_block, file, _) = quota_fixture();
    load_user_quota(&super_block, file.inode());
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(0)).unwrap();
    assert_eq!(
        Ext4QuotaIo {
            filesystem: &state,
            handle: Some(&mut handle)
        }
        .write(file.inode(), 80, b"new"),
        Err(KError::from(LinuxError::ENOSPC).canonicalize())
    );
    assert!(!handle.has_updates());
    assert!(!journal.is_aborted());
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
}

#[def_test]
fn aborted_suspended_quota_releases_its_file_without_rewriting_recovery_state() {
    use crate::superblock::test_fixtures::{
        install_test_journal, test_journal_superblock_bytes_with_blocks,
    };

    let (super_block, file, device) = internal_quota_fixture();
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let mut bytes = test_journal_superblock_bytes_with_blocks(9502, 1024);
        bytes[4..8].copy_from_slice(&4_u32.to_be_bytes());
        crate::jbd2::enable_superblock_revoke(&mut bytes).unwrap();
        let superblock =
            crate::jbd2::JournalSuperblock::decode(&bytes, 4096, 1024, [0; 16]).unwrap();
        install_test_journal(&mut state, superblock);
        assert!(state.journal_supports_revoke());
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        Ext4QuotaIo {
            filesystem: &state,
            handle: Some(&mut handle),
        }
        .write(file.inode(), 8191, &[0])
        .unwrap();
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
        state.commit_running_metadata_transaction().unwrap();
        assert!(state.pending_checkpoint_count() > 0);
        assert!(state.needs_recovery());
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let result = super_block.quota_info().get(
            &mut Ext4QuotaIo {
                filesystem: &state,
                handle: Some(&mut handle),
            },
            QuotaId::new(QuotaType::User, 0x0102_0304).unwrap(),
        );
        assert!(result.is_err());
        assert!(handle.has_updates());
        assert!(
            state
                .complete_metadata_mutation(
                    handle,
                    Err::<(), _>(crate::Ext4Error::InvalidJournalTransaction)
                )
                .is_err()
        );
        assert!(journal.is_aborted());
        state.mark_quotas_suspended(&super_block).unwrap();
        let quota = super_block.quota_info();
        let info = quota.info(QuotaType::User).unwrap();
        assert!(info.is_dirty());
        assert!(info.state().contains(QuotaState::SUSPENDED));
        assert!(state.needs_recovery());
    }
    let image = device.bytes();
    let writes = device.write_count();
    let references = Arc::strong_count(file.inode());
    assert!(Ext4SuperOperations::deactivate_internal_quotas(&super_block).is_err());
    assert!(super_block.quota_info().info(QuotaType::User).is_none());
    assert_eq!(super_block.quota_info().next_cached_id(None), None);
    assert_eq!(Arc::strong_count(file.inode()), references - 1);
    assert_eq!(device.write_count(), writes);
    assert_eq!(device.bytes(), image);
}

#[def_test]
fn chown_transfers_the_block_allocated_by_extra_isize_expansion() {
    use crate::superblock::test_fixtures::update_test_inode_extra_isize;

    let (super_block, _file, _) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let inode = allocate_accounted_inode(&mut state, 1000);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    update_test_inode_extra_isize(&mut state, &inode, 0, &mut handle);
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    let value = vec![0x71; 88];
    state
        .set_xattr(
            &inode,
            crate::Ext4XattrNamespace::User,
            b"key",
            &value,
            Ext4Timestamp::new(1, 0),
        )
        .unwrap();
    assert_eq!(inode.extra_isize(), 0);
    assert_eq!(inode.blocks(), 0);
    state
        .update_inode_metadata(
            &inode,
            crate::Ext4InodeMetadataUpdate::new(Ext4Timestamp::new(2, 0)).with_owner(2000, 2000),
        )
        .unwrap();
    let allocated = i64::from(state.layout().block_size());
    assert_eq!(inode.extra_isize(), state.superblock().want_extra_isize());
    assert_eq!(inode.blocks() * 512, allocated as u64);
    assert_eq!(owner_usage(&super_block, &mut state, 1000).used_bytes, 0);
    assert_eq!(
        owner_usage(&super_block, &mut state, 2000).used_bytes,
        allocated
    );
}

#[def_test]
fn buffered_async_writeback_accounts_allocations_once() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    let bytes = vec![0x5a; TEST_BLOCK_SIZE * 2];
    for _ in 0..2 {
        let mut offset = 0;
        assert_eq!(file.write_from(&bytes, &mut offset).unwrap(), bytes.len());
        file.fsync(false).unwrap();
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        assert_eq!(
            owner_usage(&super_block, &mut state, 1000).used_bytes,
            bytes.len() as i64
        );
        assert!(!state.journal.as_ref().unwrap().is_aborted());
    }
}

#[def_test]
fn suspension_releases_mixed_records_but_retains_the_quota_file() {
    let (super_block, file, device) = internal_quota_fixture();
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let inode = allocate_accounted_inode(&mut state, 1000);
        inode.drop_quota_records(None);
        state.initialize_owner_quotas(2000, 2000, 0).unwrap();
        state.suspend_internal_quotas(&super_block).unwrap();
        state.sync_filesystem().unwrap();
    }
    {
        let quota = super_block.quota_info();
        let info = quota.info(QuotaType::User).unwrap();
        assert!(info.state().contains(QuotaState::SUSPENDED));
        assert!(Arc::ptr_eq(info.file(), file.inode()));
        assert_eq!(quota.next_cached_id(None), None);
    }
    let writes = device.write_count();
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
    assert_eq!(device.write_count(), writes);
    assert!(super_block.quota_info().info(QuotaType::User).is_none());
}

#[def_test]
fn suspension_waits_for_eviction_to_release_quota_reservations() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let private = inode.downcast::<Ext4Inode>().unwrap();
    ext4_private(&super_block)
        .unwrap()
        .read()
        .reserve_delalloc_range_into(&private, LogicalBlock::new(0), 1)
        .unwrap();
    let writeback = private.lock_writeback_submission();
    let eviction = ktask::spawn(move || drop(inode));
    while !eviction.state().is_blocked() {
        assert_ne!(eviction.state(), ktask::TaskState::Exited);
        ktask::yield_now();
    }
    let result = Arc::new(ksync::Mutex::new(None));
    let suspension = {
        let super_block = super_block.clone();
        let result = result.clone();
        ktask::spawn(move || {
            *result.lock() = Some(
                ext4_private(&super_block)
                    .unwrap()
                    .suspend_internal_quotas(&super_block),
            );
        })
    };
    let was_blocked = loop {
        match suspension.state() {
            ktask::TaskState::Blocked => break true,
            ktask::TaskState::Exited => break false,
            _ => ktask::yield_now(),
        }
    };
    drop(writeback);
    eviction.join();
    suspension.join();
    assert!(was_blocked);
    assert_eq!(*result.lock(), Some(Ok(())));
    assert!(
        !super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .is_active()
    );
    ext4_private(&super_block)
        .unwrap()
        .read()
        .resume_internal_quotas(&super_block)
        .unwrap();
    drop(private);
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
}

#[def_test]
fn readonly_remount_rejects_writers_and_unlinked_files_before_suspending_quota() {
    let (super_block, quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let number = inode.private::<Ext4Inode>().unwrap().number();
    let path = inode_path(&super_block, &inode);
    let writer = kvfs::dentry_open(
        path.clone(),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert_eq!(
        remount_quota_filesystem(&super_block, true),
        Err(VfsError::from(LinuxError::EBUSY))
    );
    drop(writer);
    let reader =
        kvfs::dentry_open(path, kvfs::OpenFlags::empty().bits(), kcred::initial_cred()).unwrap();
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let credits = state.quota_metadata_credits(JournalCredits::new(64));
        let journal = state
            .metadata_journal_without_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .unwrap();
        let mut handle = journal.begin(credits).unwrap();
        let private = inode.private::<Ext4Inode>().unwrap();
        state.add_namespace_orphan(private, &mut handle).unwrap();
        state
            .update_unlinked_inode_metadata(private, None, Ext4Timestamp::new(1, 0), &mut handle)
            .unwrap();
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
    }
    assert_eq!(
        remount_quota_filesystem(&super_block, true),
        Err(VfsError::from(LinuxError::EBUSY))
    );
    assert!(!super_block.flags().contains(SuperBlockFlags::RDONLY));
    assert!(
        !super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .state()
            .contains(QuotaState::SUSPENDED)
    );
    drop(reader);
    drop(inode);
    assert!(
        !ext4_private(&super_block)
            .unwrap()
            .read()
            .is_inode_allocated(number)
            .unwrap()
    );
    remount_quota_filesystem(&super_block, true).unwrap();
    assert!(
        super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .state()
            .contains(QuotaState::SUSPENDED)
    );
    assert!(Arc::ptr_eq(
        super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .file(),
        quota_file.inode()
    ));
    remount_quota_filesystem(&super_block, false).unwrap();
    assert!(
        !super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .state()
            .contains(QuotaState::SUSPENDED)
    );
}

#[def_test(serial)]
fn writable_open_waits_for_readonly_remount_policy_publication() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let path = inode_path(&super_block, &inode);
    let state = ext4_private(&super_block).unwrap().write(&super_block);
    let remount_result = Arc::new(ksync::Mutex::new(None));
    let saved_result = remount_result.clone();
    let target = super_block.clone();
    let remounter = ktask::spawn(move || {
        *saved_result.lock() = Some(remount_quota_filesystem(&target, true));
    });
    while !remounter.state().is_blocked() {
        ktask::yield_now();
    }
    let open_result = Arc::new(ksync::Mutex::new(None));
    let saved_result = open_result.clone();
    let opener = ktask::spawn(move || {
        *saved_result.lock() = Some(kvfs::dentry_open(
            path,
            kvfs::OpenFlags::WRITE_ONLY.bits(),
            kcred::initial_cred(),
        ));
    });
    while !opener.state().is_blocked() {
        ktask::yield_now();
    }
    assert_eq!(inode.write_count(), 0);
    drop(state);
    remounter.join();
    opener.join();
    assert_eq!(remount_result.lock().take().unwrap(), Ok(()));
    assert!(
        matches!(open_result.lock().take().unwrap(), Err(error) if error == VfsError::ReadOnlyFilesystem)
    );
    remount_quota_filesystem(&super_block, false).unwrap();
}

#[def_test]
fn regular_inode_publication_does_not_acquire_mount_mutation() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let private = Arc::try_unwrap(allocate_accounted_inode(&mut state, 1000))
        .ok()
        .unwrap();
    let number = u64::from(private.number().get());
    let inode = Ext4SuperOperations::iget_from_private(&super_block, private).unwrap();
    let mut is_registered = false;
    super_block
        .for_each_inode(|inode| {
            is_registered |= inode.inode() == number;
            Ok(())
        })
        .unwrap();
    assert!(is_registered);
    drop(state);
    assert_eq!(inode.inode(), number);
}

#[def_test]
fn failed_extra_isize_expansion_does_not_publish_chown() {
    use crate::superblock::test_fixtures::update_test_inode_extra_isize;

    let (super_block, _quota_file, _) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let inode = allocate_accounted_inode(&mut state, 1000);
    state.initialize_owner_quotas(2000, 2000, 0).unwrap();
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    update_test_inode_extra_isize(&mut state, &inode, 0, &mut handle);
    let block = state.inode_table_entry_block(inode.number()).unwrap();
    let mut bytes = state.read_metadata_block(block).unwrap().as_ref().to_vec();
    let decoded = state
        .update_referenced_inode_table_entry(&mut bytes, &inode, |entry| {
            entry[128..132].copy_from_slice(&0xea03_0000_u32.to_le_bytes());
            Ok(())
        })
        .unwrap();
    let access = state.metadata_io.write_access(block, &mut handle).unwrap();
    crate::superblock::replace_metadata_access_bytes(&access, bytes).unwrap();
    state.publish_inode_metadata(&inode, decoded.metadata).unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
    assert_eq!(
        state.update_inode_metadata(
            &inode,
            crate::Ext4InodeMetadataUpdate::new(Ext4Timestamp::new(2, 0)).with_owner(2000, 2000),
        ),
        Err(crate::Ext4Error::Corrupt(crate::CorruptKind::InvalidXattr))
    );
    assert_eq!((inode.stat().uid, inode.stat().gid), (1000, 1000));
}

#[def_test]
fn project_readonly_recovery_cleans_orphan_state() {
    use crate::{
        disk::ReadOnlyCompatFeatures,
        superblock::test_fixtures::recovery_orphan_file_test_filesystem,
    };

    let (_, original) = recovery_orphan_file_test_filesystem();
    let mut image = original.bytes();
    let offset = 1024 + 0x64;
    let flags = u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap())
        | ReadOnlyCompatFeatures::PROJECT.bits()
        | ReadOnlyCompatFeatures::ORPHAN_PRESENT.bits();
    image[offset..offset + 4].copy_from_slice(&flags.to_le_bytes());
    let device = Arc::new(TestDevice::new(image.clone()));
    let mut state = Ext4SbInfo::open(crate::io::test_block_device(device.clone()), true).unwrap();
    let replay = state.prepare_recovery().unwrap();
    state.finish_recovery(replay).unwrap();
    assert!(!state.has_orphan_present());
    assert_eq!(state.orphan_file_empty(), Ok(true));
    state.sync_filesystem().unwrap();
    assert!(!state.has_orphan_present());
    assert!(device.write_count() > 0);
    assert_ne!(device.bytes(), image);
}

#[def_test]
fn writable_remount_leaves_readonly_inode_owners_uncached() {
    let (super_block, quota_file, device) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    remount_quota_filesystem(&super_block, true).unwrap();
    let journal = ext4_private(&super_block)
        .unwrap()
        .read()
        .journal
        .as_ref()
        .unwrap()
        .clone();
    let blocker = journal.begin(JournalCredits::new(1)).unwrap();
    let writes = device.write_count();
    remount_quota_filesystem(&super_block, false).unwrap();
    assert!(!super_block.flags().contains(SuperBlockFlags::RDONLY));
    {
        let quota = super_block.quota_info();
        let info = quota.info(QuotaType::User).unwrap();
        assert!(!info.state().contains(QuotaState::SUSPENDED));
        assert!(Arc::ptr_eq(info.file(), quota_file.inode()));
        assert!(!quota.is_cached_active(QuotaId::new(QuotaType::User, 1000).unwrap()));
    }
    assert_eq!(device.write_count(), writes);
    drop(blocker);
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert!(
        super_block
            .quota_info()
            .is_cached_active(QuotaId::new(QuotaType::User, 1000).unwrap())
    );
    drop(file);
}

#[def_test]
fn owner_initialization_reuses_sparse_quota_blocks_below_eof() {
    let (super_block, file, _) = internal_quota_fixture();
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let mut io = Ext4QuotaIo {
        filesystem: &state,
        handle: Some(&mut handle),
    };
    io.write(file.inode(), 32767, &[0]).unwrap();
    io.write(file.inode(), 20, &32_u32.to_le_bytes()).unwrap();
    io.write(file.inode(), 24, &24_u32.to_le_bytes()).unwrap();
    io.write(file.inode(), 28, &16_u32.to_le_bytes()).unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
    let inode = file.inode().private::<Ext4Inode>().unwrap();
    for logical in [4, 6] {
        assert!(matches!(
            state.map_blocks(inode, LogicalBlock::new(logical)).unwrap(),
            crate::BlockMapping::Hole { .. }
        ));
    }
    state
        .enable_internal_quota(&super_block, QuotaType::User, file.inode())
        .unwrap();
    state.initialize_owner_quotas(1000, 1000, 0).unwrap();
    for logical in [4, 6] {
        assert!(matches!(
            state.map_blocks(inode, LogicalBlock::new(logical)).unwrap(),
            crate::BlockMapping::Mapped { .. }
        ));
    }
    assert_eq!(owner_usage(&super_block, &mut state, 1000).used_inodes, 0);
    assert!(!state.journal.as_ref().unwrap().is_aborted());
}

#[def_test]
fn corrupt_quota_tree_aborts_before_any_record_can_be_charged() {
    let (super_block, file, _) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    Ext4QuotaIo {
        filesystem: &state,
        handle: Some(&mut handle),
    }
    .write(file.inode(), 1024, &1_u32.to_le_bytes())
    .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.sync_filesystem().unwrap();
    assert_eq!(
        state.initialize_owner_quotas(1000, 1000, 0),
        Err(crate::Ext4Error::Corrupt(crate::CorruptKind::InvalidQuota))
    );
    assert!(state.journal.as_ref().unwrap().is_aborted());
    assert!(state.ensure_writable().is_err());
}

#[def_test]
fn shared_xattr_copy_on_write_releases_the_old_owner_reference_charge() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let inode = allocate_accounted_inode(&mut state, 1000);
    state
        .set_xattr(
            &inode,
            crate::Ext4XattrNamespace::User,
            b"shared",
            &vec![1; 400],
            Ext4Timestamp::new(1, 0),
        )
        .unwrap();
    let block_size = u64::from(state.layout().block_size());
    assert_eq!(
        owner_usage(&super_block, &mut state, 1000).used_bytes,
        block_size as i64
    );
    let old_block = FilesystemBlock::new(inode.file_acl_block());
    assert!(!state.superblock().features().has_metadata_checksum());
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let mut bytes = state
        .read_metadata_block(old_block)
        .unwrap()
        .as_ref()
        .to_vec();
    // Model an existing image whose other inode retains this xattr block.
    bytes[4..8].copy_from_slice(&2_u32.to_le_bytes());
    let access = state
        .metadata_io
        .write_access(old_block, &mut handle)
        .unwrap();
    crate::superblock::replace_metadata_access_bytes(&access, bytes).unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state
        .set_xattr(
            &inode,
            crate::Ext4XattrNamespace::User,
            b"shared",
            &vec![2; 400],
            Ext4Timestamp::new(2, 0),
        )
        .unwrap();
    assert_ne!(inode.file_acl_block(), old_block.get());
    assert_eq!(inode.blocks() * 512, block_size);
    assert_eq!(
        owner_usage(&super_block, &mut state, 1000).used_bytes,
        block_size as i64
    );
    assert!(inode.sync_transaction().is_none());
    state
        .update_inode_metadata(
            &inode,
            crate::Ext4InodeMetadataUpdate::new(Ext4Timestamp::new(3, 0)).with_owner(2000, 2000),
        )
        .unwrap();
    assert!(inode.sync_transaction().is_some());
    assert_eq!(owner_usage(&super_block, &mut state, 1000).used_bytes, 0);
    assert_eq!(
        owner_usage(&super_block, &mut state, 2000).used_bytes,
        block_size as i64
    );
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    state
        .update_unlinked_inode_metadata(&inode, None, Ext4Timestamp::new(4, 0), &mut handle)
        .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    state.eviction_prepare(&inode).unwrap();
    assert_eq!(inode.file_acl_block(), 0);
    assert_eq!(inode.blocks(), 0);
    assert_eq!(state.raw_inode(inode.number()).unwrap().blocks(), 0);
    assert_eq!(owner_usage(&super_block, &mut state, 2000).used_bytes, 0);
    assert!(state.eviction_release_batch(&inode, 1).unwrap().1);
    state.eviction_finish(&inode, Ext4Timestamp::new(5, 0)).unwrap();
}

#[def_test]
fn external_xattr_rejects_invalid_accounting_before_releasing_blocks() {
    use crate::{
        CorruptKind, Ext4Error, UnsupportedKind,
        disk::features::{CompatFeatures, ReadOnlyCompatFeatures},
        superblock::test_fixtures::set_allocator_feature_bits,
    };

    let original = vec![1; 400];
    let replacement = vec![2; 400];
    for shared in [false, true] {
        for huge in [false, true] {
            for (value, eviction) in [
                (None, false),
                (Some(b"inline".as_slice()), false),
                (Some(replacement.as_slice()), false),
                (None, true),
            ] {
                // A private block can be rewritten in place and is not released.
                if !shared && value.is_some_and(|value| value.len() == replacement.len()) {
                    continue;
                }
                let (super_block, _quota_file, _) = internal_quota_fixture();
                let mut state = ext4_private(&super_block).unwrap().write(&super_block);
                state.sync_filesystem().unwrap();
                set_allocator_feature_bits(
                    &mut state,
                    CompatFeatures::empty(),
                    ReadOnlyCompatFeatures::HUGE_FILE,
                );
                state.metadata_io.invalidate_all();
                let inode = allocate_accounted_inode(&mut state, 1000);
                state
                    .set_xattr(
                        &inode,
                        crate::Ext4XattrNamespace::User,
                        b"large",
                        &original,
                        Ext4Timestamp::new(1, 0),
                    )
                    .unwrap();
                let block = FilesystemBlock::new(inode.file_acl_block());
                let journal = state.metadata_journal().unwrap();
                if shared {
                    let mut bytes = state.read_metadata_block(block).unwrap().as_ref().to_vec();
                    bytes[4..8].copy_from_slice(&2_u32.to_le_bytes());
                    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
                    let access = state.metadata_io.write_access(block, &mut handle).unwrap();
                    crate::superblock::replace_metadata_access_bytes(&access, bytes).unwrap();
                    state.complete_metadata_mutation(handle, Ok(())).unwrap();
                }
                inode.disable_extra_isize_expansion();
                let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
                if huge {
                    state
                        .update_inode_flags_timestamps_metadata(
                            &inode,
                            inode.flags() | crate::disk::inode::EXT4_HUGE_FILE_FL,
                            Ext4Timestamp::new(2, 0),
                            &mut handle,
                        )
                        .unwrap();
                } else {
                    inode
                        .update_allocated_blocks(-i128::from(inode.blocks()), |_| Ok(()))
                        .unwrap();
                    state.update_inode_blocks_metadata(&inode, &mut handle).unwrap();
                }
                state.complete_metadata_mutation(handle, Ok(())).unwrap();
                if eviction {
                    inode.set_links_count(0);
                }

                let group = state.block_group_for_block(block).unwrap();
                let bitmap = FilesystemBlock::new(state.group_geometry(group).unwrap().block_bitmap());
                let before_bitmap = state.read_metadata_block(bitmap).unwrap().as_ref().to_vec();
                let before_block = state.read_metadata_block(block).unwrap().as_ref().to_vec();
                let before_free = state.free_blocks_count();
                let before_blocks = inode.blocks();
                let before_quota = owner_usage(&super_block, &mut state, 1000).used_bytes;
                assert_eq!(before_quota, i64::from(state.layout().block_size()));
                let result = if eviction {
                    state.eviction_prepare(&inode)
                } else if let Some(value) = value {
                    state.set_xattr(
                        &inode,
                        crate::Ext4XattrNamespace::User,
                        b"large",
                        value,
                        Ext4Timestamp::new(3, 0),
                    )
                } else {
                    state.remove_xattr(
                        &inode,
                        crate::Ext4XattrNamespace::User,
                        b"large",
                        Ext4Timestamp::new(3, 0),
                    )
                };
                let expected = if huge {
                    Ext4Error::Unsupported(UnsupportedKind::HugeFile)
                } else if eviction {
                    Ext4Error::Corrupt(CorruptKind::InvalidInode)
                } else {
                    Ext4Error::Overflow
                };
                assert_eq!(result, Err(expected));
                assert!(!journal.is_aborted());
                assert!(state.ensure_writable().is_ok());
                assert_eq!(inode.file_acl_block(), block.get());
                assert_eq!(inode.blocks(), before_blocks);
                assert!(inode.orphan_location().is_none());
                assert_eq!(state.free_blocks_count(), before_free);
                assert_eq!(
                    state.read_metadata_block(bitmap).unwrap().as_ref(),
                    before_bitmap.as_slice()
                );
                assert_eq!(
                    state.read_metadata_block(block).unwrap().as_ref(),
                    before_block.as_slice()
                );
                assert_eq!(owner_usage(&super_block, &mut state, 1000).used_bytes, before_quota);
                assert_eq!(
                    state.get_xattr(&inode, crate::Ext4XattrNamespace::User, b"large"),
                    Ok(Some(original.clone()))
                );
            }
        }
    }
}

#[def_test]
fn fallocate_accounts_unwritten_blocks_once_and_transfers_them_on_chown() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    let length = 2 * u64::from(TEST_BLOCK_SIZE as u32);
    for _ in 0..2 {
        file.fallocate(kvfs::FallocateFlags::KEEP_SIZE, 0, length)
            .unwrap();
    }
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    assert_eq!(
        owner_usage(&super_block, &mut state, 1000).used_bytes,
        length as i64
    );
    let private = inode.private::<Ext4Inode>().unwrap();
    assert_eq!(private.size(), 0);
    assert_eq!(private.blocks() * 512, length);
    state
        .update_inode_metadata(
            private,
            crate::Ext4InodeMetadataUpdate::new(Ext4Timestamp::new(3, 0)).with_owner(2000, 2000),
        )
        .unwrap();
    assert_eq!(owner_usage(&super_block, &mut state, 1000).used_bytes, 0);
    assert_eq!(
        owner_usage(&super_block, &mut state, 2000).used_bytes,
        length as i64
    );
}

#[def_test]
fn preallocation_append_claims_quota_and_truncate_preserves_another_files_usage() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let other = accounted_vfs_inode(&super_block, 1000);
    let other_file = kvfs::dentry_open(
        inode_path(&super_block, &other),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    other_file
        .fallocate(kvfs::FallocateFlags::empty(), 0, TEST_BLOCK_SIZE as u64)
        .unwrap();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    let bytes = vec![0x5a; TEST_BLOCK_SIZE * 4];
    let mut offset = 0;
    assert_eq!(file.write_from(&bytes, &mut offset), Ok(bytes.len()));
    file.fsync(false).unwrap();
    let private = inode.private::<Ext4Inode>().unwrap();
    let hint = private
        .prealloc_map()
        .update(|map| map.get(&LogicalBlock::new(4)).copied())
        .unwrap();
    assert_eq!(file.write_from(&bytes, &mut offset), Ok(bytes.len()));
    file.fsync(false).unwrap();
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let mapping = state.map_blocks(private, LogicalBlock::new(4)).unwrap();
        assert!(
            matches!(mapping, BlockMapping::Mapped { physical, .. } if physical.get() == hint.phys_start.get())
        );
        let usage = owner_usage(&super_block, &mut state, 1000);
        assert_eq!(
            (usage.used_bytes, usage.reserved_bytes),
            (9 * TEST_BLOCK_SIZE as i64, 0)
        );
        assert_eq!(state.delalloc_reserved_blocks(), 0);
    }
    file.truncate(0).unwrap();
    drop(file);
    drop(other_file);
    remount_quota_filesystem(&super_block, true).unwrap();
    remount_quota_filesystem(&super_block, false).unwrap();
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    let usage = owner_usage(&super_block, &mut state, 1000);
    assert_eq!(
        (usage.used_inodes, usage.used_bytes, usage.reserved_bytes),
        (2, TEST_BLOCK_SIZE as i64, 0)
    );
    assert_eq!(
        other.private::<Ext4Inode>().unwrap().blocks() * 512,
        TEST_BLOCK_SIZE as u64
    );
}

#[def_test]
fn preallocation_quota_admission_precedes_consumption_without_delalloc() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = accounted_vfs_inode(&super_block, 1000);
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::WRITE_ONLY.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    let bytes = vec![0x5a; TEST_BLOCK_SIZE * 4];
    assert_eq!(file.write_from(&bytes, &mut 0), Ok(bytes.len()));
    file.fsync(false).unwrap();
    let private = inode.private::<Ext4Inode>().unwrap();
    let hint = private
        .prealloc_map()
        .update(|map| map.get(&LogicalBlock::new(4)).copied())
        .unwrap();
    for allowed_blocks in [4, 8] {
        {
            let mut state = ext4_private(&super_block).unwrap().write(&super_block);
            let credits = JournalCredits::new(1);
            let journal = state.metadata_journal().unwrap();
            let mut handle = journal.begin(credits).unwrap();
            let mut io = Ext4QuotaIo {
                filesystem: &state,
                handle: Some(&mut handle),
            };
            let mut quota = super_block.quota_info();
            let id = QuotaId::new(QuotaType::User, 1000).unwrap();
            quota
                .set(
                    &mut io,
                    id,
                    kquota::Dqblk {
                        space_hard_limit_bytes: allowed_blocks * TEST_BLOCK_SIZE as i64,
                        ..kquota::Dqblk::default()
                    },
                    kquota::QuotaFields::SPACE_HARD,
                    1,
                )
                .unwrap();
            quota.set_limits_enabled(QuotaType::User, true).unwrap();
            quota.sync_dquot(&mut io, id).unwrap();
            drop(quota);
            state.complete_metadata_mutation(handle, Ok(())).unwrap();
        }
        let result = file.fallocate(
            kvfs::FallocateFlags::KEEP_SIZE,
            bytes.len() as u64,
            bytes.len() as u64,
        );
        if allowed_blocks == 4 {
            assert_eq!(result, Err(VfsError::from(LinuxError::EDQUOT)));
            assert_eq!(
                private
                    .prealloc_map()
                    .update(|map| map.get(&LogicalBlock::new(4)).copied()),
                Some(hint)
            );
        } else {
            result.unwrap();
            let state = ext4_private(&super_block).unwrap().read();
            assert!(
                matches!(state.map_blocks(private, LogicalBlock::new(4)).unwrap(), BlockMapping::Unwritten { physical, .. } if physical.get() == hint.phys_start.get())
            );
        }
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let usage = owner_usage(&super_block, &mut state, 1000);
        assert_eq!(
            (usage.used_bytes, usage.reserved_bytes),
            (allowed_blocks * TEST_BLOCK_SIZE as i64, 0)
        );
        assert!(!state.journal.as_ref().unwrap().is_aborted());
    }
}

#[def_test]
fn cold_owner_readonly_lookup_survives_journal_abort_without_quota_io() {
    let (super_block, _quota_file, device) = internal_quota_fixture();
    let number = {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let inode = allocate_accounted_inode(&mut state, 1000);
        inode.set_size(4);
        state
            .writeback_ordered_at_with_prealloc_budget(
                &inode,
                0,
                b"data",
                4,
                crate::Ext4SyncIntent::FullMetadata,
                0,
                || false,
            )
            .unwrap();
        state.sync_filesystem().unwrap();
        inode.drop_quota_records(None);
        {
            let mut quota = super_block.quota_info();
            quota.suspend_cached(QuotaType::User).unwrap();
            quota.resume(QuotaType::User).unwrap();
        }
        state.fail_journal_operation(Ext4Error::Device(block::DriverError::Io));
        assert!(state.journal.as_ref().unwrap().is_aborted());
        inode.number()
    };
    let id = QuotaId::new(QuotaType::User, 1000).unwrap();
    assert!(!super_block.quota_info().is_cached_active(id));
    let writes = device.write_count();
    let inode = Ext4SuperOperations::iget(&super_block, number).unwrap();
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::empty().bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert_eq!(file.path().getattr().unwrap().size, 4);
    let mut output = [0; 4];
    assert_eq!(file.read_from(&mut output, &mut 0), Ok(4));
    assert_eq!(&output, b"data");
    assert!(matches!(
        kvfs::dentry_open(
            inode_path(&super_block, &inode),
            kvfs::OpenFlags::WRITE_ONLY.bits(),
            kcred::initial_cred(),
        ),
        Err(VfsError::ReadOnlyFilesystem)
    ));
    assert_eq!(
        file.path().truncate(0, &kcred::initial_cred()),
        Err(VfsError::ReadOnlyFilesystem)
    );
    assert!(!super_block.quota_info().is_cached_active(id));
    assert_eq!(device.write_count(), writes);
}

#[def_test]
fn cold_readonly_inode_initializes_quota_at_xattr_mutation() {
    let (super_block, _quota_file, device) = internal_quota_fixture();
    let number = {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let inode = allocate_accounted_inode(&mut state, 1000);
        inode.drop_quota_records(None);
        state.suspend_internal_quotas(&super_block).unwrap();
        state.resume_internal_quotas(&super_block).unwrap();
        state.sync_filesystem().unwrap();
        inode.number()
    };
    let id = QuotaId::new(QuotaType::User, 1000).unwrap();
    let writes = device.write_count();
    let inode = Ext4SuperOperations::iget(&super_block, number).unwrap();
    let file = kvfs::dentry_open(
        inode_path(&super_block, &inode),
        kvfs::OpenFlags::empty().bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert!(!super_block.quota_info().is_cached_active(id));
    assert_eq!(device.write_count(), writes);
    let mut state = ext4_private(&super_block).unwrap().write(&super_block);
    state
        .set_xattr(
            file.inode().private().unwrap(),
            Ext4XattrNamespace::User,
            b"large",
            &vec![0x5a; 1024],
            Ext4Timestamp::new(3, 0),
        )
        .unwrap();
    assert!(super_block.quota_info().is_cached_active(id));
    let usage = owner_usage(&super_block, &mut state, 1000);
    assert_eq!(
        (usage.used_inodes, usage.used_bytes),
        (1, TEST_BLOCK_SIZE as i64)
    );
}

#[def_test]
fn delalloc_space_refusal_refunds_quota_and_preserves_remount_admission() {
    let (super_block, _quota_file, _) = internal_quota_fixture();
    let inode = {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let inode = allocate_accounted_inode(&mut state, 1000);
        assert_eq!(
            state.reserve_delalloc_range_into(
                &inode,
                LogicalBlock::new(0),
                state.free_blocks_count() + 1,
            ),
            Err(Ext4Error::NoSpace)
        );
        assert_eq!(state.delalloc_reserved_blocks(), 0);
        assert_eq!(inode.reserved_data_blocks(), 0);
        let usage = owner_usage(&super_block, &mut state, 1000);
        assert_eq!((usage.used_bytes, usage.reserved_bytes), (0, 0));
        inode.drop_quota_records(None);
        inode
    };
    remount_quota_filesystem(&super_block, true).unwrap();
    remount_quota_filesystem(&super_block, false).unwrap();
    assert_eq!(inode.reserved_data_blocks(), 0);
}

#[def_test]
fn invalid_quota_numbers_fail_activation_before_record_io() {
    let (super_block, _quota_file, device) = internal_quota_fixture();
    Ext4SuperOperations::deactivate_internal_quotas(&super_block).unwrap();
    let writes = device.write_count();
    for (field, number) in [(0x240, 2_u32), (0x240, 11), (0x244, 3), (0x244, 11)] {
        {
            let mut state = ext4_private(&super_block).unwrap().write(&super_block);
            let (block, offset, length) = state.primary_superblock_location().unwrap();
            let mut bytes = state.read_metadata_block(block).unwrap().as_ref().to_vec();
            bytes[offset + field..offset + field + 4].copy_from_slice(&number.to_le_bytes());
            state.superblock =
                crate::disk::Ext4DiskSuperblock::decode(&bytes[offset..offset + length]).unwrap();
            assert_eq!(
                state.is_quota_inode(InodeNumber::new(number)),
                number == crate::disk::inode::EXT4_USR_QUOTA_INO
            );
        }
        assert_eq!(
            Ext4SuperOperations::activate_internal_quotas(&super_block),
            Err(VfsError::from(LinuxError::EUCLEAN))
        );
        assert!(super_block.quota_info().info(QuotaType::User).is_none());
        assert!(super_block.quota_info().info(QuotaType::Group).is_none());
        assert_eq!(device.write_count(), writes);
    }
}

#[def_test]
fn external_quota_control_retains_one_file_and_restores_inode_policy() {
    use kvfs::QuotaOperations;

    let (super_block, file, _) = quota_fixture();
    let inode = file.inode();
    load_user_quota(&super_block, inode);
    let retained = super_block.quota_info().unload(QuotaType::User).unwrap();
    drop(retained);
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        state.vfs_superblock = Arc::downgrade(&super_block);
        state.quota_mount_options = 1;
    }
    let writer = kvfs::dentry_open(
        file.path().clone(),
        kvfs::OpenFlags::READ_WRITE.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    let mut position = 100;
    assert_eq!(writer.write_from(&[9, 8, 7, 6], &mut position), Ok(4));
    let ops = &EXT4_SUPER_OPERATIONS;
    ops.quota_on(
        &super_block,
        QuotaType::User,
        QuotaFormat::VfsV1.id(),
        inode,
    )
    .unwrap();
    let mut marker = [0; 4];
    let mut position = 100;
    assert_eq!(file.read_from(&mut marker, &mut position), Ok(4));
    assert_eq!(marker, [9, 8, 7, 6]);
    assert!(inode.flags().contains(
        kvfs::NodeFlags::NO_QUOTA | kvfs::NodeFlags::IMMUTABLE | kvfs::NodeFlags::NO_ATIME
    ));
    assert!(Arc::ptr_eq(
        super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .file(),
        inode
    ));
    assert_eq!(
        inode.permission(kvfs::Permission::MAY_WRITE, &kcred::initial_cred()),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        ops.quota_on(
            &super_block,
            QuotaType::User,
            QuotaFormat::VfsV1.id(),
            inode
        ),
        Err(LinuxError::EBUSY.into())
    );
    assert_eq!(
        writer.write(b"ordinary write"),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(writer.write(&[]), Err(VfsError::OperationNotPermitted));
    assert_eq!(
        writer.write_from(b"x", &mut writer.max_file_size()),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(writer.truncate(0), Err(VfsError::OperationNotPermitted));
    assert_eq!(
        writer.page_mkwrite(0, |_| Ok(())),
        Err(VfsError::OperationNotPermitted)
    );
    let id = QuotaId::new(QuotaType::User, 1234).unwrap();
    let value = kquota::Dqblk {
        space_hard_limit_bytes: 8192,
        inode_hard_limit: 7,
        ..kquota::Dqblk::default()
    };
    ops.set(
        &super_block,
        id,
        value,
        kquota::QuotaFields::SPACE_HARD | kquota::QuotaFields::INODE_HARD,
    )
    .unwrap();
    assert_eq!(
        ops.get(&super_block, id).unwrap().space_hard_limit_bytes,
        8192
    );
    assert_eq!(ops.next(&super_block, QuotaType::User, 1).unwrap().0, id);
    assert_eq!(
        ops.next(&super_block, QuotaType::User, 1235)
            .map_err(LinuxError::from),
        Err(LinuxError::ENOENT)
    );
    ops.set_grace_times(&super_block, QuotaType::User, 19, 23)
        .unwrap();
    // An unused record is deleted during final release, after the pre-off sync.
    let empty_id = QuotaId::new(QuotaType::User, 5678).unwrap();
    ops.get(&super_block, empty_id).unwrap();
    ops.quota_off(&super_block, QuotaType::User).unwrap();
    assert!(super_block.quota_info().info(QuotaType::User).is_none());
    let mut expected = vec![0; inode.size() as usize];
    {
        let state = ext4_private(&super_block).unwrap().read();
        let io = Ext4QuotaIo {
            filesystem: &state,
            handle: None,
        };
        assert_eq!(io.read(inode, 0, &mut expected).unwrap(), expected.len());
    }
    let mut ordinary = vec![0; expected.len()];
    assert_eq!(
        file.read_from(&mut ordinary, &mut 0).unwrap(),
        expected.len()
    );
    assert_eq!(
        ordinary, expected,
        "ordinary I/O must see the final released quota tree"
    );
    assert!(!inode.flags().intersects(
        kvfs::NodeFlags::NO_QUOTA | kvfs::NodeFlags::IMMUTABLE | kvfs::NodeFlags::NO_ATIME
    ));
    ops.quota_on(
        &super_block,
        QuotaType::User,
        QuotaFormat::VfsV1.id(),
        inode,
    )
    .unwrap();
    assert_eq!(ops.get(&super_block, id).unwrap().inode_hard_limit, 7);
    assert_eq!(
        super_block
            .quota_info()
            .info(QuotaType::User)
            .unwrap()
            .space_grace_seconds(),
        19
    );
    ops.quota_off(&super_block, QuotaType::User).unwrap();
}

#[def_test]
fn malformed_external_quota_header_does_not_abort_the_filesystem() {
    use kvfs::QuotaOperations;

    let (super_block, file, _) = quota_fixture();
    let inode = file.inode();
    load_user_quota(&super_block, inode);
    drop(super_block.quota_info().unload(QuotaType::User).unwrap());
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let mut io = Ext4QuotaIo {
            filesystem: &state,
            handle: Some(&mut handle),
        };
        // Block one is the required root; a file claiming only one block has
        // no room for that root despite its otherwise valid magic/version.
        io.write(inode, 20, &1_u32.to_le_bytes()).unwrap();
        state.complete_metadata_mutation(handle, Ok(())).unwrap();
        state.sync_filesystem().unwrap();
        state.quota_mount_options = 1;
    }
    let result = EXT4_SUPER_OPERATIONS.quota_on(
        &super_block,
        QuotaType::User,
        QuotaFormat::VfsV1.id(),
        inode,
    );
    assert_eq!(result.map_err(LinuxError::from), Err(LinuxError::EUCLEAN));
    assert!(super_block.quota_info().info(QuotaType::User).is_none());
    assert!(!inode.flags().contains(kvfs::NodeFlags::NO_QUOTA));
    assert!(!super_block.is_readonly());
    assert!(
        !ext4_private(&super_block)
            .unwrap()
            .read()
            .journal
            .as_ref()
            .unwrap()
            .is_aborted()
    );
}

#[def_test]
fn readonly_quota_io_has_no_write_authority() {
    let (super_block, file, device) = quota_fixture();
    let writes = device.write_count();
    let state = ext4_private(&super_block).unwrap().read();
    let mut io = Ext4QuotaIo {
        filesystem: &state,
        handle: None,
    };
    assert_eq!(
        io.write(file.inode(), 0, b"data"),
        Err(LinuxError::EIO.into())
    );
    assert_eq!(device.write_count(), writes);
}
