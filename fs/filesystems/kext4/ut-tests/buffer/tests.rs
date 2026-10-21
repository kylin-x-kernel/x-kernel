// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};
use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;
use crate::{
    Ext4Error, FilesystemBlock, UnsupportedKind,
    io::FilesystemDevice,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
};

pub(super) const TEST_BLOCK_SIZE: usize = 512;

pub(super) struct TestDevice {
    bytes: crate::sync::Mutex<Box<[u8]>>,
    read_count: AtomicUsize,
    write_count: AtomicUsize,
    flush_count: AtomicUsize,
    failures_remaining: AtomicUsize,
    delay: TimeSpan,
}

impl TestDevice {
    pub(super) fn new(block_count: usize, failures: usize, delay: TimeSpan) -> Self {
        let mut bytes = vec![0; block_count * TEST_BLOCK_SIZE];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index as u8;
        }
        Self {
            bytes: crate::sync::Mutex::new(bytes.into_boxed_slice()),
            read_count: AtomicUsize::new(0),
            write_count: AtomicUsize::new(0),
            flush_count: AtomicUsize::new(0),
            failures_remaining: AtomicUsize::new(failures),
            delay,
        }
    }

    fn byte_at(&self, offset: usize) -> u8 {
        self.bytes.lock()[offset]
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
        (self.bytes.lock().len() / TEST_BLOCK_SIZE) as u64
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
        ktask::sleep(self.delay);
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(TEST_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        let bytes = self.bytes.lock();
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
        let mut bytes = self.bytes.lock();
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

pub(super) fn cache_for(device: Arc<TestDevice>) -> MetadataBlockCache {
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

#[def_test]
fn repeated_reads_share_one_cached_block_and_pins_prevent_reclaim() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn targeted_reclaim_preserves_pins_and_unselected_cached_blocks() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
    let cache = cache_for(device.clone());
    let pinned = cache.read(FilesystemBlock::new(1)).unwrap();
    drop(cache.read(FilesystemBlock::new(2)).unwrap());
    drop(cache.read(FilesystemBlock::new(3)).unwrap());
    assert_eq!(
        cache.reclaim_blocks(&[FilesystemBlock::new(1), FilesystemBlock::new(2)]),
        1
    );
    assert_eq!(cache.cached_block_count(), 2);
    drop(cache.read(FilesystemBlock::new(3)).unwrap());
    assert_eq!(device.read_count.load(Ordering::Relaxed), 3);
    drop(pinned);
    assert_eq!(cache.reclaim_blocks(&[FilesystemBlock::new(1)]), 1);
    assert_eq!(cache.cached_block_count(), 1);
}

#[def_test]
fn incremental_reclaim_bounds_inspection_and_revisits_released_pins() {
    let device = Arc::new(TestDevice::new(5, 0, TimeSpan::ZERO));
    let cache = cache_for(device.clone());
    let pinned = cache.read(FilesystemBlock::new(1)).unwrap();
    let access = cache
        .write_access(FilesystemBlock::new(2), TransactionId::new(9))
        .unwrap();
    access.mark_dirty().unwrap();
    drop(access);
    for block in [3, 4] {
        drop(cache.read(FilesystemBlock::new(block)).unwrap());
    }

    assert_eq!(cache.reclaim_unused_incremental(0), 0);
    assert_eq!(cache.reclaim_unused_incremental(2), 0);
    assert_eq!(cache.cached_block_count(), 4);
    // The budget counts inspected slots, so the next visit reaches clean slots
    // beyond the pinned and dirty entries rather than starting over at them.
    assert_eq!(cache.reclaim_unused_incremental(2), 2);
    assert_eq!(cache.cached_block_count(), 2);
    drop(pinned);
    // The cursor's last key was removed; wrapping still revisits the old pin.
    assert_eq!(cache.reclaim_unused_incremental(2), 1);
    assert_eq!(cache.cached_block_count(), 1);
    assert_eq!(
        cache.buffer_state(FilesystemBlock::new(2)).unwrap(),
        MetadataBufferState::Dirty(TransactionId::new(9))
    );
    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 0);
}

#[def_test]
fn incremental_reclaim_finishes_a_sweep_despite_new_higher_keys() {
    let device = Arc::new(TestDevice::new(6, 0, TimeSpan::ZERO));
    let cache = cache_for(device);
    let old_pin = cache.read(FilesystemBlock::new(1)).unwrap();
    drop(cache.read(FilesystemBlock::new(2)).unwrap());
    drop(cache.read(FilesystemBlock::new(3)).unwrap());
    assert_eq!(cache.reclaim_unused_incremental(1), 0);
    drop(old_pin);
    let first_new_pin = cache.read(FilesystemBlock::new(4)).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    let second_new_pin = cache.read(FilesystemBlock::new(5)).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    // The old sweep ended at block 3. Visit the released block 1 before the
    // newly appended pins instead of indefinitely following growing keys.
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    assert_eq!(cache.cached_block_count(), 2);
    drop(first_new_pin);
    drop(second_new_pin);
    assert_eq!(cache.reclaim_unused_incremental(2), 2);
}

#[def_test]
fn incremental_reclaim_preserves_frozen_images_until_checkpoint_and_unpin() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
    let cache = cache_for(device);
    let block = FilesystemBlock::new(1);
    let transaction = TransactionId::new(13);
    let access = cache.write_access(block, transaction).unwrap();
    access.mark_dirty().unwrap();
    drop(access);
    let frozen = cache.journal_commit_block(block, transaction).unwrap();
    drop(cache.read(FilesystemBlock::new(2)).unwrap());

    assert_eq!(cache.reclaim_unused_incremental(4), 1);
    assert_eq!(cache.cached_block_count(), 1);
    cache.checkpoint_block(block, transaction).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(4), 0);
    drop(frozen);
    assert_eq!(cache.reclaim_unused_incremental(4), 1);
    assert_eq!(cache.reclaim_unused_incremental(4), 0);
}

#[def_test]
fn cache_invalidation_resets_incremental_reclaim_position() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
    let cache = cache_for(device);
    let old_pin = cache.read(FilesystemBlock::new(1)).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(1), 0);
    cache.invalidate_all();
    drop(cache.read(FilesystemBlock::new(0)).unwrap());
    let new_pin = cache.read(FilesystemBlock::new(2)).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    drop(old_pin);
    drop(new_pin);
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
}

#[def_test]
fn cache_pressure_reclaims_unrelated_clean_blocks_without_checkpoints() {
    let retained = cache::METADATA_CACHE_RETAINED_BYTES / TEST_BLOCK_SIZE;
    let device = Arc::new(TestDevice::new(retained + 256, 0, TimeSpan::ZERO));
    let cache = cache_for(device.clone());
    let io = Ext4MetadataIo {
        cache: cache.clone(),
    };
    let pinned = cache.read(FilesystemBlock::new(1)).unwrap();
    let transaction = TransactionId::new(17);
    for block in [2, 3] {
        let access = cache
            .write_access(FilesystemBlock::new(block), transaction)
            .unwrap();
        access.mark_dirty().unwrap();
    }
    let frozen = cache
        .journal_commit_block(FilesystemBlock::new(3), transaction)
        .unwrap();
    for block in 4..retained + 132 {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    assert!(cache.cached_block_count() > retained);
    // No transaction retires and no full scan runs. Cache demand alone drives
    // bounded progress, while dirty, externally pinned and frozen data survive.
    for _ in 0..4 {
        assert!(io.reclaim_cache(64) <= 64);
    }
    assert!(cache.cached_block_count() <= retained);
    assert_eq!(io.reclaim_cache(64), 0);
    assert_eq!(
        cache.buffer_state(FilesystemBlock::new(2)).unwrap(),
        MetadataBufferState::Dirty(transaction)
    );
    assert_eq!(
        cache.buffer_state(FilesystemBlock::new(3)).unwrap(),
        MetadataBufferState::Clean
    );
    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.flush_count.load(Ordering::Relaxed), 0);
    let reads = device.read_count.load(Ordering::Relaxed);
    cache
        .checkpoint_block(FilesystemBlock::new(3), transaction)
        .unwrap();
    assert_eq!(device.write_count.load(Ordering::Relaxed), 1);
    assert_eq!(device.read_count.load(Ordering::Relaxed), reads);
    drop(frozen);
    drop(pinned);
}

#[def_test]
fn full_reclaim_restarts_the_cursor_before_new_lower_keys() {
    let cache = cache_for(Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO)));
    let first_pin = cache.read(FilesystemBlock::new(1)).unwrap();
    let last_pin = cache.read(FilesystemBlock::new(3)).unwrap();
    assert_eq!(cache.reclaim_unused_incremental(1), 0);
    assert_eq!(cache.reclaim_unused(usize::MAX), 0);
    drop(cache.read(FilesystemBlock::new(0)).unwrap());
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    drop(first_pin);
    drop(last_pin);
    assert_eq!(cache.reclaim_unused(usize::MAX), 2);
}

#[def_test]
fn cache_pressure_respects_retention_and_zero_scan_budget() {
    let retained = cache::METADATA_CACHE_RETAINED_BYTES / TEST_BLOCK_SIZE;
    let cache = cache_for(Arc::new(TestDevice::new(retained + 8, 0, TimeSpan::ZERO)));
    for block in 0..4 {
        drop(cache.read(FilesystemBlock::new(block)).unwrap());
    }
    assert_eq!(cache.reclaim_for_pressure(64), 0);
    assert_eq!(cache.cached_block_count(), 4);
    for block in 4..retained + 4 {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    // Repeated maintenance never turns a zero budget into a periodic full scan.
    for _ in 0..32 {
        assert_eq!(cache.reclaim_for_pressure(0), 0);
    }
    assert_eq!(cache.cached_block_count(), retained + 4);
    assert_eq!(cache.reclaim_for_pressure(2), 2);
    assert_eq!(cache.cached_block_count(), retained + 2);
    assert_eq!(cache.reclaim_for_pressure(2), 2);
    assert_eq!(cache.reclaim_for_pressure(2), 0);
}

#[def_test]
fn concurrent_reads_coalesce_one_device_io() {
    const READER_COUNT: usize = 4;

    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::from_millis(20)));
    let cache = cache_for(device.clone());
    let ready = Arc::new(AtomicUsize::new(0));
    let mut readers = vec![];

    for _ in 0..READER_COUNT {
        let cache = cache.clone();
        let ready = ready.clone();
        readers.push(ktask::spawn(move || {
            ready.fetch_add(1, Ordering::AcqRel);
            while ready.load(Ordering::Acquire) < READER_COUNT {
                ktask::yield_now();
            }
            let buffer = cache.read(FilesystemBlock::new(2)).unwrap();
            assert_eq!(buffer.as_ref().len(), TEST_BLOCK_SIZE);
        }));
    }
    for reader in readers {
        reader.join();
    }

    assert_eq!(device.read_count.load(Ordering::Relaxed), 1);
}

#[def_test]
fn failed_reads_are_removed_and_can_be_retried() {
    let device = Arc::new(TestDevice::new(4, 1, TimeSpan::ZERO));
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

#[def_test]
fn write_access_pins_buffer_and_dirty_state_prevents_reclaim() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn write_access_updates_cached_metadata_bytes_without_mutating_old_snapshots() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn write_access_rejects_other_running_transaction() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn create_access_skips_device_read_and_requires_full_initialization() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_returns_dirty_buffer_to_clean_reclaimable_state() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn writeback_keeps_frozen_snapshot_while_next_transaction_updates_buffer() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn writeback_failure_records_buffer_error() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_write_access_consumes_journal_handle_credit() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_write_conflict_requires_transaction_abort_after_publication() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn journal_abort_does_not_rewind_successful_prior_metadata_update() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_create_conflict_requires_transaction_abort_after_publication() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_forget_records_revoke_without_cached_buffer() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_forget_drops_cached_current_transaction_state() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_forget_without_revoke_drops_current_transaction_update() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_forget_suppresses_older_checkpoint_home_write() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_create_reuses_block_with_only_revoke_skipped_checkpoint_snapshot() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn metadata_io_forget_rejects_in_flight_checkpoint_writeback() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

    writeback.finish_checkpoint().unwrap();
    // JournalBusy must preserve the in-flight buffer. Once writeback ends,
    // the same handle can retry; a reserved revoke is idempotent.
    metadata_io
        .forget_metadata_block(block, &mut second_handle)
        .unwrap();
}

#[def_test]
fn metadata_io_forget_keeps_cached_block_when_revoke_credit_is_missing() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_committed_writes_dirty_metadata_and_flushes() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_committed_writes_frozen_snapshot_while_next_transaction_stays_dirty() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_committed_preserves_multiple_frozen_transactions_in_order() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_committed_writes_created_initialized_metadata_without_reading_old_block() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn journal_commit_blocks_capture_transaction_owned_metadata_snapshot() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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

#[def_test]
fn checkpoint_committed_releases_unchanged_journaled_buffer_without_write() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
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
