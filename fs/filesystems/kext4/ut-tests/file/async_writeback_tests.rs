// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{sync::Arc, vec};

use block::DriverError;
use unittest::def_test;

use crate::{
    BlockCount, Ext4Error, Ext4Inode, Ext4SbInfo, Ext4SyncIntent, LogicalBlock, PhysicalBlock,
    extent::ExtentMappingState,
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, TestDevice, allocate_checkpointed_regular_inode,
        allocate_contiguous_blocks, install_test_internal_journal,
        journal_allocator_test_filesystem,
    },
};

fn mapped_overwrite_fixture() -> (Ext4SbInfo, Ext4Inode, Arc<TestDevice>) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let setup = JournalTransactions::new(TransactionId::new(931));
    let mut handle = setup.begin(JournalCredits::new(32)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 3, &mut handle);
    for (logical, relative, state) in [
        (0, 0, ExtentMappingState::Initialized),
        (1, 2, ExtentMappingState::Initialized),
        (3, 1, ExtentMappingState::Unwritten),
    ] {
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                PhysicalBlock::new(physical.get() + relative),
                BlockCount::new(1),
                state,
                &mut handle,
            )
            .unwrap();
    }
    inode.set_size((4 * TEST_BLOCK_SIZE) as u64);
    filesystem
        .update_regular_inode_write_metadata(
            &inode,
            inode.size(),
            RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = setup.force_commit(TransactionId::new(931)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup.finish_checkpoint_for_test(&commit).unwrap();
    install_test_internal_journal(&mut filesystem, 932);
    (filesystem, inode, device)
}

#[def_test]
fn mapped_overwrite_probe_falls_back_before_io_for_holes_unwritten_and_growth() {
    let (filesystem, inode, device) = mapped_overwrite_fixture();
    let _writeback = inode.lock_writeback();
    let input = vec![0x58; 4 * TEST_BLOCK_SIZE];
    let writes = device.write_count();
    for (offset, data, visible_size) in [
        (0, input.as_slice(), inode.size()),
        (17, &input[..3], inode.size()),
        (
            (3 * TEST_BLOCK_SIZE) as u64,
            &input[..TEST_BLOCK_SIZE],
            inode.size(),
        ),
        (
            0,
            &input[..TEST_BLOCK_SIZE],
            inode.size() + TEST_BLOCK_SIZE as u64,
        ),
    ] {
        assert!(
            filesystem
                .prepare_mapped_writeback(
                    &inode,
                    offset,
                    data,
                    visible_size,
                    Ext4SyncIntent::FullMetadata,
                )
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(device.write_count(), writes);
    assert_eq!(filesystem.pending_checkpoint_count(), 0);
}

#[def_test]
fn mapped_bio_failure_reports_prefix_and_drains_later_requests() {
    let (filesystem, inode, device) = mapped_overwrite_fixture();
    let _writeback = inode.lock_writeback();
    let writes = device.write_count();
    device.fail_write_at(writes + 1);
    let input = vec![0x79; 2 * TEST_BLOCK_SIZE];
    let pending = filesystem
        .prepare_mapped_writeback(&inode, 0, &input, inode.size(), Ext4SyncIntent::DataOnly)
        .unwrap()
        .unwrap()
        .submit(input)
        .unwrap();
    // Both discontiguous runs are submitted, even though the first fails inline.
    assert_eq!(device.write_count(), writes + 2);
    let failure = pending.wait().unwrap_err();
    assert_eq!(failure.error(), Ext4Error::Device(DriverError::Io));
    assert_eq!(failure.completed_bytes(), 0);
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    filesystem
        .read_at(&inode, TEST_BLOCK_SIZE as u64, &mut bytes)
        .unwrap();
    assert!(bytes.iter().all(|byte| *byte == 0x79));
}
