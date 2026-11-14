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
        allocate_contiguous_blocks, fragment_ordered_writeback_free_space,
        install_credit_limited_journal, install_test_internal_journal,
        journal_allocator_test_filesystem, recovery_test_filesystem_with_free_blocks,
    },
};

struct FinishDropProbe(Arc<ksync::Mutex<alloc::vec::Vec<&'static str>>>);

impl FinishDropProbe {
    fn record_call(&self) {
        self.0.lock().push("call");
    }
}

impl Drop for FinishDropProbe {
    fn drop(&mut self) {
        self.0.lock().push("drop");
    }
}

#[def_test]
fn worker_preparation_failure_leaves_queue_unpublished_and_retryable() {
    let queue = Arc::new(crate::writeback_io::InodeWritebackIo::new());
    let result = queue.prepare_with(vec![512], |weak| {
        assert!(weak.upgrade().is_some());
        Err(kerrno::KError::NoMemory.into())
    });
    assert!(matches!(result, Err(Ext4Error::Allocation(kerrno::KError::NoMemory))));
    assert_eq!(Arc::weak_count(&queue), 0);
    assert_eq!(queue.snapshot(), 0);
    assert!(queue.has_capacity());

    let io = queue.prepare(vec![512]).unwrap();
    assert_eq!(queue.snapshot(), 0);
    let another = queue
        .prepare_with(vec![1024], |_| panic!("prepared worker must be reused"))
        .unwrap();
    drop((io, another));
}

#[def_test]
fn allocation_errors_preserve_kernel_error_mapping() {
    for error in [kerrno::KError::NoMemory, kerrno::KError::InvalidInput] {
        let ext4_error = Ext4Error::from(error);
        assert_eq!(ext4_error, Ext4Error::Allocation(error));
        assert_eq!(crate::vfs::into_vfs_err(ext4_error), error);
    }
}

#[def_test]
fn finish_owner_failure_records_error_before_capture_drop_and_allows_retry() {
    let (filesystem, inode, device) = mapped_overwrite_fixture();
    let submission = inode.lock_writeback_submission();
    let input = vec![0x79; 2 * TEST_BLOCK_SIZE];
    let queue = inode.writeback_io();
    let writes = device.write_count();
    let sequence = queue.snapshot();
    let events = Arc::new(ksync::Mutex::new(vec![]));
    let probe = FinishDropProbe(events.clone());
    let plan = filesystem
        .prepare_mapped_writeback(&inode, 0, &input, inode.size(), Ext4SyncIntent::DataOnly)
        .unwrap()
        .unwrap()
        .into_async();
    let result = plan.submit_with_storage(
        input.clone(),
        queue,
        move |_| probe.record_call(),
        |error| {
            assert_eq!(error, Ext4Error::Allocation(kerrno::KError::NoMemory));
            events.lock().push("error");
        },
        |flags| {
            assert_eq!(flags, kalloc_types::AllocFlags::NOWAIT);
            Err(kerrno::KError::NoMemory)
        },
    );
    assert_eq!(result, Err(Ext4Error::Allocation(kerrno::KError::NoMemory)));
    assert_eq!(*events.lock(), vec!["error", "drop"]);
    assert_eq!(device.write_count(), writes);
    assert_eq!(queue.snapshot(), sequence);
    assert!(queue.has_capacity());

    let retry_events = Arc::new(ksync::Mutex::new(vec![]));
    let probe = FinishDropProbe(retry_events.clone());
    filesystem
        .prepare_mapped_writeback(&inode, 0, &input, inode.size(), Ext4SyncIntent::DataOnly)
        .unwrap()
        .unwrap()
        .into_async()
        .submit(
            input,
            queue,
            move |result| {
                assert_eq!(result, Ok(2 * TEST_BLOCK_SIZE));
                probe.record_call();
            },
            |error| panic!("unexpected preparation failure: {error:?}"),
        )
        .unwrap();
    queue.drain();
    drop(submission);
    queue.flush_completions().unwrap();
    assert_eq!(device.write_count(), writes + 2);
    assert_eq!(queue.snapshot(), sequence + 1);
    assert_eq!(*retry_events.lock(), vec!["call", "drop"]);
}

fn mapped_overwrite_fixture() -> (Ext4SbInfo, Ext4Inode, Arc<TestDevice>) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let setup = JournalTransactions::new(TransactionId::new(931));
    let mut handle = setup.begin(JournalCredits::new(32)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, &inode, 3, &mut handle);
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
    let _writeback = inode.lock_writeback_submission();
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
fn data_only_overwrite_after_metadata_commit_still_requires_barrier() {
    let (mut filesystem, inode, device) = mapped_overwrite_fixture();
    filesystem
        .commit_regular_inode_write_metadata(&inode, inode.size(), RegularWriteMetadata::SizeOnly)
        .unwrap();
    filesystem
        .sync_inode(&inode, Ext4SyncIntent::FullMetadata)
        .unwrap();
    assert!(inode.sync_transaction().unwrap().is_committed());
    let _writeback = inode.lock_writeback_submission();
    let input = vec![0xa7; TEST_BLOCK_SIZE];
    let result = filesystem
        .prepare_mapped_writeback(&inode, 0, &input, inode.size(), Ext4SyncIntent::DataOnly)
        .unwrap()
        .unwrap()
        .submit(input)
        .unwrap()
        .wait();
    filesystem
        .finish_mapped_writeback(&inode, 0, result, Ext4SyncIntent::DataOnly, || true)
        .unwrap();
    let flushes = device.flush_count();
    device.fail_flush_at(flushes + 1);
    assert_eq!(
        filesystem.sync_inode(&inode, Ext4SyncIntent::DataOnly),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert_eq!(device.flush_count(), flushes + 1);
}

#[def_test]
fn mapped_bio_failure_reports_prefix_and_drains_later_requests() {
    let (filesystem, inode, device) = mapped_overwrite_fixture();
    let _writeback = inode.lock_writeback_submission();
    let writes = device.write_count();
    device.fail_write_at(writes + 1);
    let input = vec![0x79; 2 * TEST_BLOCK_SIZE];
    let completed = Arc::new(ksync::Mutex::new(None));
    let result = completed.clone();
    filesystem
        .prepare_mapped_writeback(&inode, 0, &input, inode.size(), Ext4SyncIntent::DataOnly)
        .unwrap()
        .unwrap()
        .into_async()
        .submit(
            input,
            inode.writeback_io(),
            move |completion| {
                *result.lock() = Some(completion);
            },
            |error| panic!("unexpected preparation failure: {error:?}"),
        )
        .unwrap();
    // Both discontiguous runs are submitted, even though the first fails inline.
    assert_eq!(device.write_count(), writes + 2);
    inode.writeback_io().drain();
    let failure = completed.lock().take().unwrap().unwrap_err();
    assert_eq!(failure.error(), Ext4Error::Device(DriverError::Io));
    assert_eq!(failure.completed_bytes(), 0);
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    filesystem
        .read_at(&inode, TEST_BLOCK_SIZE as u64, &mut bytes)
        .unwrap();
    assert!(bytes.iter().all(|byte| *byte == 0x79));
}

#[def_test]
fn async_fragmented_allocation_completes_through_small_journal_without_rewriting_data() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    fragment_ordered_writeback_free_space(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 933, 64, true);
    let input = vec![0x7d; TEST_BLOCK_SIZE * 8];
    inode.set_size(input.len() as u64);
    filesystem
        .reserve_delalloc_range_into(&inode, LogicalBlock::new(0), 8)
        .unwrap();
    let plan = filesystem
        .prepare_allocating_writeback(
            &inode,
            0,
            input.len(),
            inode.size(),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();
    assert_eq!(inode.disk_size(), 0);
    filesystem.sync_filesystem().unwrap();
    // One conversion fits, but reserving another path edit forces restart.
    install_credit_limited_journal(&mut filesystem, 935, 40, true);
    let physical = match filesystem.map_blocks(&inode, LogicalBlock::new(0)).unwrap() {
        crate::BlockMapping::Unwritten { physical, .. } => physical,
        _ => panic!("prepared allocation must remain unwritten"),
    };
    let completed = Arc::new(ksync::Mutex::new(None));
    let result = completed.clone();
    plan.submit(
        input.clone(),
        inode.writeback_io(),
        move |completion| {
            *result.lock() = Some(completion);
        },
        |error| panic!("unexpected preparation failure: {error:?}"),
    )
    .unwrap();
    inode.writeback_io().drain();
    device.fail_write_block(physical);
    let data = completed.lock().take().unwrap();
    assert_eq!(
        filesystem.finish_prepared_writeback(&inode, 0, data, Ext4SyncIntent::DataOnly, || true),
        Ok(input.len())
    );
    assert!(filesystem.pending_checkpoint_count() > 0);
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(inode.disk_size(), input.len() as u64);
    filesystem.sync_filesystem().unwrap();
    let mut output = vec![0; input.len()];
    filesystem.read_at(&inode, 0, &mut output).unwrap();
    assert_eq!(output, input);
}

#[def_test]
fn preparation_enospc_retains_safe_unwritten_prefix_without_submitting_data() {
    let (mut filesystem, device) = recovery_test_filesystem_with_free_blocks(2);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_credit_limited_journal(&mut filesystem, 934, 341, true);
    inode.set_size((TEST_BLOCK_SIZE * 3) as u64);
    assert!(matches!(
        filesystem.prepare_allocating_writeback(
            &inode,
            0,
            TEST_BLOCK_SIZE * 3,
            inode.size(),
            Ext4SyncIntent::DataOnly
        ),
        Err(Ext4Error::NoSpace)
    ));
    assert_eq!(inode.disk_size(), 0);
    assert!(!filesystem.journal.as_ref().unwrap().is_aborted());
    let mut output = vec![0xff; TEST_BLOCK_SIZE * 3];
    filesystem.read_at(&inode, 0, &mut output).unwrap();
    assert!(output.iter().all(|byte| *byte == 0));
    filesystem.sync_filesystem().unwrap();
    let reopened = Ext4SbInfo::open(
        crate::io::test_block_device(Arc::new(TestDevice::new(device.bytes()))),
        false,
    )
    .unwrap();
    let disk_inode = reopened.load_inode_private(inode.number()).unwrap();
    assert_eq!(disk_inode.disk_size(), 0);
    assert!(matches!(
        reopened
            .map_blocks(&disk_inode, LogicalBlock::new(0))
            .unwrap(),
        crate::BlockMapping::Unwritten { .. }
    ));
    assert!(matches!(
        reopened
            .map_blocks(&disk_inode, LogicalBlock::new(2))
            .unwrap(),
        crate::BlockMapping::Hole { .. }
    ));
}
