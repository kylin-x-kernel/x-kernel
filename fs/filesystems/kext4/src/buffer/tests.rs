// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Barrier, thread, time::Duration};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};

use super::*;
use crate::{
    Ext4Error, FilesystemBlock, UnsupportedKind,
    io::FilesystemDevice,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
};

const TEST_BLOCK_SIZE: usize = 512;

struct TestDevice {
    bytes: std::sync::Mutex<Box<[u8]>>,
    read_count: AtomicUsize,
    write_count: AtomicUsize,
    flush_count: AtomicUsize,
    failures_remaining: AtomicUsize,
    delay: Duration,
}

impl TestDevice {
    fn new(block_count: usize, failures: usize, delay: Duration) -> Self {
        let mut bytes = vec![0; block_count * TEST_BLOCK_SIZE];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index as u8;
        }
        Self {
            bytes: std::sync::Mutex::new(bytes.into_boxed_slice()),
            read_count: AtomicUsize::new(0),
            write_count: AtomicUsize::new(0),
            flush_count: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(failures),
            delay,
        }
    }

    fn byte_at(&self, offset: usize) -> u8 {
        self.bytes.lock().unwrap()[offset]
    }
}

impl Device for TestDevice {
    fn name(&self) -> &str {
        "kext4-metadata-cache-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for TestDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().unwrap().len() / TEST_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        TEST_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        if self
            .failures_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(DriverError::Io);
        }
        thread::sleep(self.delay);
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(TEST_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        let bytes = self.bytes.lock().unwrap();
        output.copy_from_slice(bytes.get(start..end).ok_or(DriverError::InvalidInput)?);
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        self.write_count.fetch_add(1, Ordering::Relaxed);
        if input.len() != TEST_BLOCK_SIZE {
            return Err(DriverError::InvalidInput);
        }
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(TEST_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        let mut bytes = self.bytes.lock().unwrap();
        bytes
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        self.flush_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn cache_for(device: Arc<TestDevice>) -> MetadataBlockCache {
    let block_count = device.num_blocks();
    let device: Arc<dyn BlockDeviceOperations> = device;
    let filesystem_device = FilesystemDevice::open(device, TEST_BLOCK_SIZE, block_count).unwrap();
    MetadataBlockCache::new(Arc::new(filesystem_device))
}

fn metadata_io_for(device: Arc<TestDevice>) -> Ext4MetadataIo {
    let block_count = device.num_blocks();
    let device: Arc<dyn BlockDeviceOperations> = device;
    let filesystem_device = FilesystemDevice::open(device, TEST_BLOCK_SIZE, block_count).unwrap();
    Ext4MetadataIo::new(Arc::new(filesystem_device))
}

#[test]
fn repeated_reads_share_one_cached_block_and_pins_prevent_reclaim() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device.clone());

    let first = cache.read(FilesystemBlock::new(1)).unwrap();
    let second = cache.read(FilesystemBlock::new(1)).unwrap();
    assert_eq!(first.as_ref()[0], 0);

    assert_eq!(device.read_count.load(Ordering::Relaxed), 1);
    assert_eq!(cache.reclaim_unused(1), 0);
    drop(first);
    drop(second);
    assert_eq!(cache.reclaim_unused(1), 1);
    assert_eq!(cache.cached_block_count(), 0);
}

#[test]
fn concurrent_reads_coalesce_one_device_io() {
    const READER_COUNT: usize = 4;

    let device = Arc::new(TestDevice::new(4, 0, Duration::from_millis(20)));
    let cache = cache_for(device.clone());
    let barrier = Arc::new(Barrier::new(READER_COUNT));
    let mut readers = vec![];

    for _ in 0..READER_COUNT {
        let cache = cache.clone();
        let barrier = barrier.clone();
        readers.push(thread::spawn(move || {
            barrier.wait();
            let buffer = cache.read(FilesystemBlock::new(2)).unwrap();
            assert_eq!(buffer.as_ref().len(), TEST_BLOCK_SIZE);
        }));
    }
    for reader in readers {
        reader.join().unwrap();
    }

    assert_eq!(device.read_count.load(Ordering::Relaxed), 1);
}

#[test]
fn failed_reads_are_removed_and_can_be_retried() {
    let device = Arc::new(TestDevice::new(4, 1, Duration::ZERO));
    let cache = cache_for(device.clone());

    assert!(matches!(
        cache.read(FilesystemBlock::new(1)),
        Err(Ext4Error::Device(DriverError::Io))
    ));
    assert_eq!(cache.cached_block_count(), 0);
    cache.read(FilesystemBlock::new(1)).unwrap();

    assert_eq!(device.read_count.load(Ordering::Relaxed), 2);
    assert_eq!(cache.cached_block_count(), 1);
}

#[test]
fn write_access_pins_buffer_and_dirty_state_prevents_reclaim() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);
    let transaction = TransactionId::new(7);

    let access = cache.write_access(block, transaction).unwrap();
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Journaled(transaction)
    );
    assert_eq!(cache.reclaim_unused(1), 0);

    access.mark_dirty().unwrap();
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(transaction)
    );
    drop(access);

    assert_eq!(cache.reclaim_unused(1), 0);
    assert_eq!(cache.cached_block_count(), 1);
}

#[test]
fn write_access_updates_cached_metadata_bytes_without_mutating_old_snapshots() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);
    let transaction = TransactionId::new(17);
    let before = cache.read(block).unwrap();

    let access = cache.write_access(block, transaction).unwrap();
    access
        .update_bytes(|old, new| {
            assert_eq!(old[0], before.as_ref()[0]);
            new[0] = 0xaa;
            new[1] = 0xbb;
            Ok(())
        })
        .unwrap();

    let after = cache.read(block).unwrap();
    assert_eq!(before.as_ref()[0], 0);
    assert_eq!(&after.as_ref()[..2], &[0xaa, 0xbb]);
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(transaction)
    );
}

#[test]
fn write_access_rejects_other_running_transaction() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);

    let first = cache
        .write_access(block, TransactionId::new(1))
        .expect("first transaction owns buffer");
    cache
        .write_access(block, TransactionId::new(1))
        .expect("same transaction can reacquire write access");
    assert!(matches!(
        cache.write_access(block, TransactionId::new(2)),
        Err(Ext4Error::Unsupported(
            UnsupportedKind::ConcurrentMetadataTransaction
        ))
    ));
    drop(first);
}

#[test]
fn create_access_skips_device_read_and_requires_full_initialization() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device.clone());
    let block = FilesystemBlock::new(3);
    let transaction = TransactionId::new(19);

    let access = cache.create_access(block, transaction).unwrap();
    assert_eq!(device.read_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Created(transaction)
    );
    assert!(matches!(
        cache.read(block),
        Err(Ext4Error::Unsupported(UnsupportedKind::MetadataBufferState))
    ));
    assert!(matches!(
        access.mark_dirty(),
        Err(Ext4Error::Unsupported(UnsupportedKind::MetadataBufferState))
    ));
    assert!(matches!(
        cache.journal_commit_block(block, transaction),
        Err(Ext4Error::Unsupported(UnsupportedKind::MetadataBufferState))
    ));

    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    bytes[0] = 0xc1;
    bytes[TEST_BLOCK_SIZE - 1] = 0x1c;
    access
        .replace_bytes(Arc::from(bytes.into_boxed_slice()))
        .unwrap();

    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(transaction)
    );
    let commit_block = cache.journal_commit_block(block, transaction).unwrap();
    assert_eq!(commit_block.target(), block);
    assert_eq!(commit_block.bytes()[0], 0xc1);
    assert_eq!(commit_block.bytes()[TEST_BLOCK_SIZE - 1], 0x1c);
}

#[test]
fn checkpoint_returns_dirty_buffer_to_clean_reclaimable_state() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);
    let transaction = TransactionId::new(11);

    let access = cache.write_access(block, transaction).unwrap();
    access.mark_dirty().unwrap();
    drop(access);

    let writeback = cache.start_writeback(block, transaction).unwrap();
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Writeback(transaction)
    );
    assert_eq!(cache.reclaim_unused(1), 0);

    writeback.finish_checkpoint().unwrap();
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Clean
    );
    assert_eq!(cache.reclaim_unused(1), 1);
    assert_eq!(cache.cached_block_count(), 0);
}

#[test]
fn writeback_keeps_frozen_snapshot_while_next_transaction_updates_buffer() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device.clone());
    let block = FilesystemBlock::new(1);
    let first_transaction = TransactionId::new(13);
    let second_transaction = TransactionId::new(14);

    let first = cache.write_access(block, first_transaction).unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[0] = 0x13;
            Ok(())
        })
        .unwrap();
    drop(first);

    let commit_snapshot = cache
        .journal_commit_block(block, first_transaction)
        .unwrap();
    assert_eq!(commit_snapshot.bytes()[0], 0x13);
    let second = cache.write_access(block, second_transaction).unwrap();
    second
        .update_bytes(|_, bytes| {
            bytes[0] = 0x14;
            Ok(())
        })
        .unwrap();
    drop(second);

    let writeback = cache
        .begin_checkpoint_for_test(block, first_transaction)
        .unwrap()
        .expect("first transaction has a frozen checkpoint image");
    assert_eq!(writeback.snapshot().as_ref()[0], 0x13);
    writeback.finish_checkpoint().unwrap();
    assert_eq!(
        cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(second_transaction)
    );
    assert_eq!(cache.read(block).unwrap().as_ref()[0], 0x14);
    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
}

#[test]
fn writeback_failure_records_buffer_error() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);
    let transaction = TransactionId::new(12);

    let access = cache.write_access(block, transaction).unwrap();
    access.mark_dirty().unwrap();
    drop(access);

    let writeback = cache.start_writeback(block, transaction).unwrap();
    writeback.fail(Ext4Error::Device(DriverError::Io));

    assert!(matches!(
        cache.read(block),
        Err(Ext4Error::Device(DriverError::Io))
    ));
    assert_eq!(cache.reclaim_unused(1), 0);
    assert_eq!(cache.cached_block_count(), 1);
}

#[test]
fn metadata_io_write_access_consumes_journal_handle_credit() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(21));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let block = FilesystemBlock::new(1);

    let access = metadata_io.write_access(block, &mut handle).unwrap();
    assert_eq!(handle.remaining_credits(), 0);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Journaled(handle.id())
    );

    assert!(matches!(
        metadata_io.write_access(FilesystemBlock::new(2), &mut handle),
        Err(Ext4Error::InsufficientJournalCredits)
    ));

    access.mark_dirty().unwrap();
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(handle.id())
    );
}

#[test]
fn metadata_io_write_conflict_requires_transaction_abort_after_publication() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let first_journal = JournalTransactions::new(TransactionId::new(31));
    let second_journal = JournalTransactions::new(TransactionId::new(41));
    let mut first = first_journal.begin(JournalCredits::new(1)).unwrap();
    let mut second = second_journal.begin(JournalCredits::new(1)).unwrap();
    let block = FilesystemBlock::new(1);

    metadata_io.write_access(block, &mut first).unwrap();
    assert!(matches!(
        metadata_io.write_access(block, &mut second),
        Err(Ext4Error::Unsupported(
            UnsupportedKind::ConcurrentMetadataTransaction
        ))
    ));
    assert_eq!(second.remaining_credits(), 0);
    assert!(second.has_updates());
    second_journal.abort(Ext4Error::InvalidJournalTransaction);
    assert!(second_journal.is_aborted());
}

#[test]
fn journal_abort_does_not_rewind_successful_prior_metadata_update() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(111));
    let first_block = FilesystemBlock::new(1);
    let second_block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first = metadata_io
        .write_access(first_block, &mut first_handle)
        .unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[7] = 0xaa;
            Ok(())
        })
        .unwrap();
    drop(first);
    first_handle.stop().unwrap();

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let second = metadata_io
        .write_access(second_block, &mut second_handle)
        .unwrap();
    second
        .update_bytes(|_, bytes| {
            bytes[7] = 0xbb;
            Ok(())
        })
        .unwrap();
    drop(second);

    journal.abort(Ext4Error::Device(DriverError::Io));
    second_handle.stop().unwrap();
    assert_eq!(
        metadata_io.read_block(first_block).unwrap().as_ref()[7],
        0xaa
    );
    assert_eq!(
        metadata_io.cache.buffer_state(first_block).unwrap(),
        MetadataBufferState::Dirty(TransactionId::new(111))
    );
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
}

#[test]
fn metadata_io_create_conflict_requires_transaction_abort_after_publication() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let first_journal = JournalTransactions::new(TransactionId::new(81));
    let second_journal = JournalTransactions::new(TransactionId::new(91));
    let mut first = first_journal.begin(JournalCredits::new(1)).unwrap();
    let mut second = second_journal.begin(JournalCredits::new(1)).unwrap();
    let block = FilesystemBlock::new(2);

    metadata_io.create_access(block, &mut first).unwrap();
    assert_eq!(first.remaining_credits(), 0);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Created(first.id())
    );

    assert!(matches!(
        metadata_io.create_access(block, &mut second),
        Err(Ext4Error::Unsupported(
            UnsupportedKind::ConcurrentMetadataTransaction
        ))
    ));
    assert_eq!(second.remaining_credits(), 0);
    assert!(second.has_updates());
    second_journal.abort(Ext4Error::InvalidJournalTransaction);
    assert!(second_journal.is_aborted());
}

#[test]
fn metadata_io_forget_records_revoke_without_cached_buffer() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(82));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(3);

    metadata_io
        .forget_metadata_block(block, &mut handle)
        .unwrap();
    assert_eq!(handle.remaining_credits(), 0);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
    assert_eq!(commit.revoked_blocks().unwrap().as_ref(), &[block]);
}

#[test]
fn metadata_io_forget_drops_cached_current_transaction_state() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(83));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(3);

    let access = metadata_io.write_access(block, &mut handle).unwrap();
    access
        .update_bytes(|_, bytes| {
            bytes[0] = 0x83;
            Ok(())
        })
        .unwrap();
    drop(access);

    metadata_io
        .forget_metadata_block(block, &mut handle)
        .unwrap();
    assert_eq!(metadata_io.cache.cached_block_count(), 0);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
    assert_eq!(commit.revoked_blocks().unwrap().as_ref(), &[block]);
    assert!(
        metadata_io
            .journal_commit_blocks(&commit)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn metadata_io_forget_without_revoke_drops_current_transaction_update() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(88));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(3);

    let access = metadata_io.write_access(block, &mut handle).unwrap();
    access
        .update_bytes(|_, bytes| {
            bytes[0] = 0x88;
            Ok(())
        })
        .unwrap();
    drop(access);

    metadata_io
        .forget_metadata_block_without_revoke(block, &mut handle)
        .unwrap();
    assert_eq!(handle.remaining_credits(), 1);
    assert_eq!(metadata_io.cache.cached_block_count(), 0);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
    assert!(commit.revoked_blocks().unwrap().as_ref().is_empty());
    assert!(
        metadata_io
            .journal_commit_blocks(&commit)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn metadata_io_forget_suppresses_older_checkpoint_home_write() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(85));
    let block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first_transaction = first_handle.id();
    let first = metadata_io.write_access(block, &mut first_handle).unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[0] = 0x85;
            Ok(())
        })
        .unwrap();
    drop(first);
    drop(first_handle);
    let first_commit = journal.force_commit(first_transaction).unwrap();
    journal.start_checkpoint_for_test(&first_commit).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&first_commit).unwrap()[0].bytes()[0],
        0x85
    );

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let second_transaction = second_handle.id();
    metadata_io
        .forget_metadata_block(block, &mut second_handle)
        .unwrap();
    drop(second_handle);

    let second_commit = journal.force_commit(second_transaction).unwrap();
    assert_eq!(second_commit.revoked_blocks().unwrap().as_ref(), &[block]);
    metadata_io.checkpoint_committed(&first_commit).unwrap();
    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 1);
    journal.finish_checkpoint_for_test(&first_commit).unwrap();
}

#[test]
fn metadata_io_create_reuses_block_with_only_revoke_skipped_checkpoint_snapshot() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(86));
    let block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first_transaction = first_handle.id();
    let first = metadata_io.write_access(block, &mut first_handle).unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[0] = 0x86;
            Ok(())
        })
        .unwrap();
    drop(first);
    drop(first_handle);
    let first_commit = journal.force_commit(first_transaction).unwrap();
    journal.start_checkpoint_for_test(&first_commit).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&first_commit).unwrap()[0].bytes()[0],
        0x86
    );

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let second_transaction = second_handle.id();
    metadata_io
        .forget_metadata_block(block, &mut second_handle)
        .unwrap();
    drop(second_handle);
    let second_commit = journal.force_commit(second_transaction).unwrap();
    journal.start_checkpoint_for_test(&second_commit).unwrap();
    assert_eq!(second_commit.revoked_blocks().unwrap().as_ref(), &[block]);

    let mut third_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let third_transaction = third_handle.id();
    let third = metadata_io.create_access(block, &mut third_handle).unwrap();
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    bytes[0] = 0x87;
    third
        .replace_bytes(Arc::from(bytes.into_boxed_slice()))
        .unwrap();
    drop(third);
    drop(third_handle);
    let third_commit = journal.force_commit(third_transaction).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&third_commit).unwrap()[0].bytes()[0],
        0x87
    );

    metadata_io.checkpoint_committed(&first_commit).unwrap();
    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    journal.finish_checkpoint_for_test(&first_commit).unwrap();
    metadata_io.checkpoint_committed(&second_commit).unwrap();
    journal.finish_checkpoint_for_test(&second_commit).unwrap();
    metadata_io.checkpoint_committed(&third_commit).unwrap();
    journal.finish_checkpoint_for_test(&third_commit).unwrap();

    let device_offset = usize::try_from(block.get()).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(device.byte_at(device_offset), 0x87);
    assert_eq!(device.write_count.load(Ordering::Relaxed), 1);
}

#[test]
fn metadata_io_forget_rejects_in_flight_checkpoint_writeback() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(87));
    let block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first_transaction = first_handle.id();
    let first = metadata_io.write_access(block, &mut first_handle).unwrap();
    first.mark_dirty().unwrap();
    drop(first);
    drop(first_handle);
    let first_commit = journal.force_commit(first_transaction).unwrap();
    metadata_io.journal_commit_blocks(&first_commit).unwrap();
    let writeback = metadata_io
        .cache
        .begin_checkpoint_for_test(block, first_transaction)
        .unwrap()
        .expect("first transaction has an in-flight checkpoint writeback");

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    assert_eq!(
        metadata_io.forget_metadata_block(block, &mut second_handle),
        Err(Ext4Error::JournalBusy)
    );
    assert_eq!(second_handle.remaining_credits(), 1);

    writeback.finish_checkpoint().unwrap();
}

#[test]
fn metadata_io_forget_keeps_cached_block_when_revoke_credit_is_missing() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let block = FilesystemBlock::new(3);
    let _cached = metadata_io.read_block(block).unwrap();
    let journal = JournalTransactions::new(TransactionId::new(84));
    let mut handle = journal.begin(JournalCredits::new(0)).unwrap();

    assert_eq!(
        metadata_io.forget_metadata_block(block, &mut handle),
        Err(Ext4Error::InsufficientJournalCredits)
    );
    assert_eq!(metadata_io.cache.cached_block_count(), 1);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Clean
    );
}

#[test]
fn checkpoint_committed_writes_dirty_metadata_and_flushes() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(51));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(2);

    let access = metadata_io.write_access(block, &mut handle).unwrap();
    access
        .update_bytes(|_, bytes| {
            bytes[0] = 0x5a;
            bytes[TEST_BLOCK_SIZE - 1] = 0xa5;
            Ok(())
        })
        .unwrap();
    drop(access);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    metadata_io.checkpoint_committed(&commit).unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let device_offset = usize::try_from(block.get()).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(device.byte_at(device_offset), 0x5a);
    assert_eq!(device.byte_at(device_offset + TEST_BLOCK_SIZE - 1), 0xa5);
    assert_eq!(device.write_count.load(Ordering::Relaxed), 1);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Clean
    );
}

#[test]
fn checkpoint_committed_writes_frozen_snapshot_while_next_transaction_stays_dirty() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(52));
    let block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first_transaction = first_handle.id();
    let first = metadata_io.write_access(block, &mut first_handle).unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[0] = 0x52;
            Ok(())
        })
        .unwrap();
    drop(first);
    drop(first_handle);
    let first_commit = journal.force_commit(first_transaction).unwrap();
    journal.start_checkpoint_for_test(&first_commit).unwrap();
    let first_blocks = metadata_io.journal_commit_blocks(&first_commit).unwrap();
    assert_eq!(first_blocks[0].bytes()[0], 0x52);

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let second_transaction = second_handle.id();
    let second = metadata_io.write_access(block, &mut second_handle).unwrap();
    second
        .update_bytes(|_, bytes| {
            bytes[0] = 0x53;
            Ok(())
        })
        .unwrap();
    drop(second);

    metadata_io.checkpoint_committed(&first_commit).unwrap();
    let device_offset = usize::try_from(block.get()).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(device.byte_at(device_offset), 0x52);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(second_transaction)
    );

    drop(second_handle);
    let second_commit = journal.force_commit(second_transaction).unwrap();
    let second_blocks = metadata_io.journal_commit_blocks(&second_commit).unwrap();
    assert_eq!(second_blocks[0].bytes()[0], 0x53);
    metadata_io.checkpoint_committed(&second_commit).unwrap();
    assert_eq!(device.byte_at(device_offset), 0x53);
}

#[test]
fn checkpoint_committed_preserves_multiple_frozen_transactions_in_order() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(53));
    let block = FilesystemBlock::new(2);

    let mut first_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let first_transaction = first_handle.id();
    let first = metadata_io.write_access(block, &mut first_handle).unwrap();
    first
        .update_bytes(|_, bytes| {
            bytes[0] = 0x53;
            Ok(())
        })
        .unwrap();
    drop(first);
    drop(first_handle);
    let first_commit = journal.force_commit(first_transaction).unwrap();
    journal.start_checkpoint_for_test(&first_commit).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&first_commit).unwrap()[0].bytes()[0],
        0x53
    );

    let mut second_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let second_transaction = second_handle.id();
    let second = metadata_io.write_access(block, &mut second_handle).unwrap();
    second
        .update_bytes(|_, bytes| {
            bytes[0] = 0x54;
            Ok(())
        })
        .unwrap();
    drop(second);
    drop(second_handle);
    let second_commit = journal.force_commit(second_transaction).unwrap();
    journal.start_checkpoint_for_test(&second_commit).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&second_commit).unwrap()[0].bytes()[0],
        0x54
    );

    let mut third_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let third_transaction = third_handle.id();
    let third = metadata_io.write_access(block, &mut third_handle).unwrap();
    third
        .update_bytes(|_, bytes| {
            bytes[0] = 0x55;
            Ok(())
        })
        .unwrap();
    drop(third);

    assert_eq!(
        metadata_io.checkpoint_committed(&second_commit),
        Err(Ext4Error::JournalBusy)
    );
    metadata_io.checkpoint_committed(&first_commit).unwrap();
    let device_offset = usize::try_from(block.get()).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(device.byte_at(device_offset), 0x53);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(third_transaction)
    );

    metadata_io.checkpoint_committed(&second_commit).unwrap();
    assert_eq!(device.byte_at(device_offset), 0x54);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Dirty(third_transaction)
    );

    drop(third_handle);
    let third_commit = journal.force_commit(third_transaction).unwrap();
    assert_eq!(
        metadata_io.journal_commit_blocks(&third_commit).unwrap()[0].bytes()[0],
        0x55
    );
    metadata_io.checkpoint_committed(&third_commit).unwrap();
    assert_eq!(device.byte_at(device_offset), 0x55);
}

#[test]
fn checkpoint_committed_writes_created_initialized_metadata_without_reading_old_block() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(101));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(2);

    let access = metadata_io.create_access(block, &mut handle).unwrap();
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    bytes[0] = 0xca;
    bytes[TEST_BLOCK_SIZE - 1] = 0xfe;
    access
        .replace_bytes(Arc::from(bytes.into_boxed_slice()))
        .unwrap();
    drop(access);
    drop(handle);

    assert_eq!(device.read_count.load(Ordering::Relaxed), 0);

    let commit = journal.force_commit(transaction).unwrap();
    metadata_io.checkpoint_committed(&commit).unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let device_offset = usize::try_from(block.get()).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(device.byte_at(device_offset), 0xca);
    assert_eq!(device.byte_at(device_offset + TEST_BLOCK_SIZE - 1), 0xfe);
    assert_eq!(device.read_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.write_count.load(Ordering::Relaxed), 1);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Clean
    );
}

#[test]
fn journal_commit_blocks_capture_transaction_owned_metadata_snapshot() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device);
    let journal = JournalTransactions::new(TransactionId::new(71));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(2);

    let access = metadata_io.write_access(block, &mut handle).unwrap();
    access
        .update_bytes(|_, bytes| {
            bytes[0] = 0x71;
            bytes[1] = 0x72;
            Ok(())
        })
        .unwrap();
    drop(access);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    let blocks = metadata_io.journal_commit_blocks(&commit).unwrap();

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].target(), block);
    assert_eq!(&blocks[0].bytes()[..2], &[0x71, 0x72]);
}

#[test]
fn checkpoint_committed_releases_unchanged_journaled_buffer_without_write() {
    let device = Arc::new(TestDevice::new(4, 0, Duration::ZERO));
    let metadata_io = metadata_io_for(device.clone());
    let journal = JournalTransactions::new(TransactionId::new(61));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    let block = FilesystemBlock::new(2);

    metadata_io.write_access(block, &mut handle).unwrap();
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    metadata_io.checkpoint_committed(&commit).unwrap();

    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        metadata_io.cache.buffer_state(block).unwrap(),
        MetadataBufferState::Clean
    );
}
