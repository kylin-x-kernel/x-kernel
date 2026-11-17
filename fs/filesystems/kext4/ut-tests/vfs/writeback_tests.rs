// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount-unlocked mapped writeback and PageCache failure contracts.

use alloc::{sync::Weak, vec};

use block::{BioOperations, BlockDeviceOperations, Device, DeviceKind, DriverError};
use kvfs::OpenFlags;
use unittest::def_test;

use super::*;
use crate::{
    io::FilesystemDevice,
    superblock::test_fixtures::{
        TEST_FREE_BLOCKS, TestDevice, allocate_checkpointed_regular_inode,
        install_credit_limited_journal, journal_allocator_test_filesystem,
        recovery_test_filesystem_with_free_blocks,
    },
    sync::Mutex,
};

struct OverwriteDevice {
    storage: Arc<TestDevice>,
    mount: Mutex<Weak<SuperBlock>>,
    writes: Mutex<usize>,
    fail_writes: Mutex<bool>,
    fail_data_reads: Mutex<bool>,
    fail_data_write_at: Mutex<Option<usize>>,
    delayed: Mutex<Vec<block::Bio>>,
    delay_writes: Mutex<bool>,
    hold_writes: Mutex<bool>,
    data_blocks: Vec<u64>,
    timestamp_update: Mutex<Option<(Weak<Ext4Inode>, Ext4Timestamp)>>,
}

impl Device for OverwriteDevice {
    fn name(&self) -> &str {
        "kext4-unlocked-overwrite-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for OverwriteDevice {
    fn num_blocks(&self) -> u64 {
        self.storage.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.storage.block_size()
    }
}

impl block::BioOperations for OverwriteDevice {
    fn submit_bio(&self, bio: block::Bio) {
        let block = bio.sector() / (self.block_size() as u64 / 512);
        if bio.operation() == block::BioOperation::Read
            && self.data_blocks.contains(&block)
            && *self.fail_data_reads.lock()
        {
            bio.end_io(Err(DriverError::Io));
            return;
        }
        if bio.operation() == block::BioOperation::Write && self.data_blocks.contains(&block) {
            let mount = self.mount.lock().upgrade().unwrap();
            let ext4 = ext4_private(&mount).unwrap();
            // Enter a competing mutation at the actual device submission boundary.
            // Holding either mount guard over I/O would deadlock this callback.
            drop(ext4.write(&mount));
            let update = self.timestamp_update.lock().take();
            if let Some((inode, timestamp)) = update {
                // Same metadata mutation used by setattr, after writeback
                // preparation and before data completion.
                ext4.write(&mount)
                    .update_inode_metadata(
                        &inode.upgrade().unwrap(),
                        crate::Ext4InodeMetadataUpdate::new(timestamp).with_mtime(timestamp),
                    )
                    .unwrap();
            }
            *self.writes.lock() += 1;
            if *self.hold_writes.lock() {
                self.delayed.lock().push(bio);
                return;
            }
            if *self.delay_writes.lock() {
                let mut delayed = self.delayed.lock();
                delayed.push(bio);
                if delayed.len() != 4 {
                    return;
                }
                let requests = core::mem::take(&mut *delayed);
                drop(delayed);
                // Complete in reverse order, failing the third submitted BIO.
                // The old synchronous writer would block on the first request.
                for (index, bio) in requests.into_iter().enumerate().rev() {
                    if index == 2 {
                        bio.end_io(Err(DriverError::Io));
                    } else {
                        self.storage.submit_bio(bio);
                    }
                }
                return;
            }
            if *self.fail_writes.lock()
                || *self.fail_data_write_at.lock() == Some(*self.writes.lock())
            {
                bio.end_io(Err(DriverError::Io));
                return;
            }
        }
        self.storage.submit_bio(bio);
    }
}

fn overwrite_fixture_with_pages(
    pages: usize,
) -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<OverwriteDevice>) {
    overwrite_fixture_with_len(PAGE_SIZE_4K * pages)
}

fn overwrite_fixture_with_len(len: usize) -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<OverwriteDevice>) {
    let pages = len.div_ceil(PAGE_SIZE_4K);
    let (mut filesystem, storage) = if pages == 0 {
        recovery_test_filesystem_with_free_blocks(TEST_FREE_BLOCKS + 64)
    } else {
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS + 64, 0b0011_1111)
    };
    let private = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_credit_limited_journal(&mut filesystem, 8082, 341, true);
    private.set_size(len as u64);
    filesystem
        .writeback_ordered_at(
            &private,
            0,
            &vec![0x41; len],
            len as u64,
            Ext4SyncIntent::DataOnly,
            || false,
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    let mut data_blocks: Vec<u64> = (0..pages)
        .map(|logical| {
            match filesystem
                .map_blocks(&private, LogicalBlock::new(logical as u64))
                .unwrap()
            {
                BlockMapping::Mapped { physical, .. } => physical.get(),
                _ => panic!("fixture must map every data block"),
            }
        })
        .collect();
    if pages == 0 {
        // This fixture reserves blocks 0..7 for fixed metadata and 16..1040
        // for its journal. Capture future data allocations, not journal writes.
        data_blocks = (7..storage.num_blocks())
            .filter(|block| !(16..1040).contains(block))
            .collect();
    }
    let device = Arc::new(OverwriteDevice {
        storage,
        mount: Mutex::new(Weak::new()),
        writes: Mutex::new(0),
        fail_writes: Mutex::new(false),
        fail_data_reads: Mutex::new(false),
        fail_data_write_at: Mutex::new(None),
        delayed: Mutex::new(Vec::new()),
        delay_writes: Mutex::new(false),
        hold_writes: Mutex::new(false),
        data_blocks,
        timestamp_update: Mutex::new(None),
    });
    filesystem.device = Arc::new(
        FilesystemDevice::open(
            crate::io::test_block_device(device.clone()),
            PAGE_SIZE_4K,
            device.num_blocks(),
        )
        .unwrap(),
    );
    let (mount, file) = read_tests::cached_file(
        filesystem,
        Arc::into_inner(private).expect("fixture released prealloc owners"),
        SuperBlockFlags::empty(),
        OpenFlags::READ_WRITE,
    );
    *device.mount.lock() = Arc::downgrade(&mount);
    (mount, file, device)
}

#[def_test]
fn buffered_growth_and_overwrite_preserve_eof_contents_and_reservations() {
    let (mount, file, _device) = overwrite_fixture_with_len(3);
    let private = file.inode().private::<Ext4Inode>().unwrap();
    assert_eq!(file.inode().size(), 3);
    assert_eq!(private.reserved_data_blocks(), 0);

    let mut expected = vec![0x51; 2 * PAGE_SIZE_4K];
    assert_eq!(file.write_from(&expected, &mut 0), Ok(expected.len()));
    assert_eq!(file.inode().size(), expected.len() as u64);
    assert_eq!(private.reserved_data_blocks(), 1);
    assert_eq!(
        private.reserved_delalloc_in_range(LogicalBlock::new(1), 1),
        Ok(1)
    );
    let mut output = vec![0; expected.len() + 1];
    assert_eq!(file.read_from(&mut output, &mut 0), Ok(expected.len()));
    assert_eq!(&output[..expected.len()], expected.as_slice());

    let overwrite = vec![0x62; PAGE_SIZE_4K];
    assert_eq!(file.write_from(&overwrite, &mut 0), Ok(overwrite.len()));
    expected[..overwrite.len()].copy_from_slice(&overwrite);
    assert_eq!(file.inode().size(), expected.len() as u64);
    assert_eq!(private.reserved_data_blocks(), 1);
    assert_eq!(
        private.reserved_delalloc_in_range(LogicalBlock::new(1), 1),
        Ok(1)
    );
    assert_eq!(file.read_from(&mut output, &mut 0), Ok(expected.len()));
    assert_eq!(&output[..expected.len()], expected.as_slice());

    file.fsync(false).unwrap();
    assert_eq!(private.disk_size(), expected.len() as u64);
    assert_eq!(private.reserved_data_blocks(), 0);
    assert_eq!(
        ext4_private(&mount)
            .unwrap()
            .read()
            .read_at(private, 0, &mut output),
        Ok(expected.len())
    );
    assert_eq!(&output[..expected.len()], expected.as_slice());
}

#[def_test]
fn cached_buffered_overwrite_does_not_wait_for_mount_persistence() {
    let (mount, file, device) = overwrite_fixture_with_pages(1);
    let input = vec![0x71; PAGE_SIZE_4K];
    file.write_from(&input, &mut 0).unwrap();
    file.fsync(false).unwrap();
    let flushes = device.storage.flush_count();
    let input = vec![0x72; PAGE_SIZE_4K];
    {
        // A journal executor may hold mount while waiting for storage. A
        // resident initialized overwrite needs neither mapping nor allocation.
        let _state = ext4_private(&mount).unwrap().write(&mount);
        let mut offset = 0;
        assert_eq!(file.write_from(&input, &mut offset), Ok(input.len()));
        assert_eq!(offset, input.len() as u64);
    }
    assert_eq!(device.storage.flush_count(), flushes);
    file.fsync(false).unwrap();
    assert!(device.storage.flush_count() > flushes);
    let mut output = vec![0; input.len()];
    let private = file.inode().private::<Ext4Inode>().unwrap();
    assert_eq!(
        ext4_private(&mount)
            .unwrap()
            .read()
            .read_at(private, 0, &mut output),
        Ok(input.len())
    );
    assert_eq!(output, input);
}

#[def_test]
fn mapped_writepages_releases_mount_protection_and_preserves_sync_barriers() {
    for data_only in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        let input = vec![0x7b; PAGE_SIZE_4K];
        assert_eq!(file.write_from(&input, &mut 0), Ok(input.len()));
        let flushes = device.storage.flush_count();
        assert_eq!(file.mapping().writepages(), Ok(()));
        assert_eq!(*device.writes.lock(), 1);
        assert_eq!(device.storage.flush_count(), flushes);
        file.mapping()
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
        let private = file.inode().private::<Ext4Inode>().unwrap();
        let mut output = vec![0; input.len()];
        assert_eq!(
            ext4_private(&mount)
                .unwrap()
                .read()
                .read_at(private, 0, &mut output),
            Ok(input.len())
        );
        assert_eq!(output, input);
        assert!(private.has_dirty_timestamps());
        assert_eq!(file.fsync(data_only), Ok(()));
        assert_eq!(private.has_dirty_timestamps(), data_only);
        // Initialized data needs no reachability metadata or ordered-data
        // precommit barrier. Full sync activates recovery and the log, then
        // its timestamp-only commit flushes the shared device; datasync flushes
        // directly while leaving those timestamps dirty.
        assert_eq!(
            device.storage.flush_count() - flushes,
            if data_only { 1 } else { 3 }
        );
        if !data_only {
            assert_eq!(file.write_from(&input, &mut 0), Ok(input.len()));
            let flushes = device.storage.flush_count();
            assert_eq!(file.mapping().writepages(), Ok(()));
            assert_eq!(device.storage.flush_count(), flushes);
            assert_eq!(file.fsync(false), Ok(()));
            // The active journal needs only the timestamp commit's device
            // flush; no allocation/conversion dependency was introduced.
            assert_eq!(device.storage.flush_count() - flushes, 1);
        }
    }
}

#[def_test]
fn clean_inode_overwrite_retains_data_barrier_after_an_earlier_commit() {
    for fail_barrier in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        let input = vec![0x57; PAGE_SIZE_4K];
        file.write_from(&input, &mut 0).unwrap();
        let inode = file.inode().downcast::<Ext4Inode>().unwrap();
        {
            // A metadata operation can stage/commit the timestamps before this
            // dirty folio is written. Its earlier barrier cannot cover later I/O.
            let mut state = ext4_private(&mount).unwrap().write(&mount);
            state.write_inode_timestamps(&inode).unwrap();
            state
                .sync_inode(&inode, Ext4SyncIntent::FullMetadata)
                .unwrap();
        }
        assert!(!inode.has_dirty_timestamps());
        let flushes = device.storage.flush_count();
        file.mapping().writepages().unwrap();
        assert_eq!(device.storage.flush_count(), flushes);
        if fail_barrier {
            device.storage.fail_flush_at(flushes + 1);
            assert_eq!(file.fsync(false), Err(VfsError::Io));
            assert!(mount.flags().contains(SuperBlockFlags::RDONLY));
        } else {
            let writes = device.storage.write_count();
            file.fsync(false).unwrap();
            // The metadata target already committed. Persist later data with
            // one barrier, without writing an otherwise empty journal commit.
            assert_eq!(device.storage.flush_count() - flushes, 1);
            assert_eq!(device.storage.write_count(), writes);
            let mut output = vec![0; PAGE_SIZE_4K];
            ext4_private(&mount)
                .unwrap()
                .read()
                .read_at(&inode, 0, &mut output)
                .unwrap();
            assert_eq!(output, input);
        }
    }
}

#[def_test]
fn mapped_writepages_data_failure_preserves_dirty_folio_without_aborting_journal() {
    let (mount, file, device) = overwrite_fixture_with_pages(1);
    assert_eq!(
        file.write_from(&vec![0x9c; PAGE_SIZE_4K], &mut 0),
        Ok(PAGE_SIZE_4K)
    );
    *device.fail_writes.lock() = true;
    assert_eq!(file.mapping().writepages(), Err(VfsError::Io));
    file.mapping()
        .with_folio(0, |folio| {
            let folio = folio.unwrap();
            assert!(folio.is_dirty());
            assert!(!folio.is_under_writeback());
        })
        .unwrap();
    let ext4 = ext4_private(&mount).unwrap().read();
    assert!(!ext4.journal.as_ref().unwrap().is_aborted());
    assert!(!mount.flags().contains(SuperBlockFlags::RDONLY));
    assert_eq!(ext4.pending_checkpoint_count(), 0);
}

#[def_test]
fn range_fsync_writes_intersecting_folios_and_keeps_siblings_dirty() {
    for data_only in [false, true] {
        let (_mount, file, device) = overwrite_fixture_with_pages(3);
        file.write_from(&vec![0x72; PAGE_SIZE_4K * 3], &mut 0)
            .unwrap();
        let flushes = device.storage.flush_count();
        // A byte in the middle page selects its whole folio, without writing
        // either neighbour. Filesystem persistence follows data completion.
        let start = PAGE_SIZE_4K as u64 + 17;
        assert_eq!(file.fsync_range(start..start + 1, data_only), Ok(()));
        assert_eq!(*device.writes.lock(), 1);
        assert!(device.storage.flush_count() > flushes);
        for page in 0..3 {
            file.mapping()
                .with_folio(page, |folio| {
                    let folio = folio.unwrap();
                    assert_eq!(folio.is_dirty(), page != 1);
                    assert!(!folio.is_under_writeback());
                })
                .unwrap();
        }
        let flushes = device.storage.flush_count();
        assert_eq!(file.fsync_range(start..start, data_only), Ok(()));
        assert_eq!(*device.writes.lock(), 1);
        assert!(device.storage.flush_count() > flushes);
    }
}

#[def_test]
fn fsync_data_failure_is_retryable_for_both_metadata_intents() {
    for data_only in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        file.write_from(&vec![0x9c; PAGE_SIZE_4K], &mut 0).unwrap();
        *device.fail_writes.lock() = true;
        assert_eq!(file.fsync(data_only), Err(VfsError::Io));
        assert!(
            !ext4_private(&mount)
                .unwrap()
                .read()
                .journal
                .as_ref()
                .unwrap()
                .is_aborted()
        );
        assert!(!mount.flags().contains(SuperBlockFlags::RDONLY));
        *device.fail_writes.lock() = false;
        assert_eq!(file.fsync(data_only), Ok(()));
        file.mapping()
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
    }
}

#[def_test]
fn mapped_writepages_pipelines_owned_bios_and_drains_out_of_order_failure() {
    let (_mount, file, device) = overwrite_fixture_with_pages(7);
    for page in [0, 2, 4, 6] {
        file.write_from(
            &vec![0x61 + page as u8; PAGE_SIZE_4K],
            &mut ((page * PAGE_SIZE_4K) as u64),
        )
        .unwrap();
    }
    *device.delay_writes.lock() = true;
    assert_eq!(file.mapping().writepages(), Err(VfsError::Io));
    assert_eq!(*device.writes.lock(), 4);
    assert!(device.delayed.lock().is_empty());
    for page in [0, 2, 4, 6] {
        file.mapping()
            .with_folio(page, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_under_writeback());
                assert_eq!(folio.is_dirty(), page == 4);
            })
            .unwrap();
    }
    *device.delay_writes.lock() = false;
    assert_eq!(file.mapping().writepages(), Ok(()));
    assert_eq!(*device.writes.lock(), 5);
}

#[def_test]
fn mapped_writeback_drains_later_batches_after_a_second_window_failure() {
    let (mount, file, device) = overwrite_fixture_with_pages(19);
    for page in (0..19).step_by(2) {
        file.write_from(
            &vec![0x5a; PAGE_SIZE_4K],
            &mut ((page * PAGE_SIZE_4K) as u64),
        )
        .unwrap();
    }
    *device.fail_data_write_at.lock() = Some(5);
    assert_eq!(file.mapping().writepages(), Err(VfsError::Io));
    assert_eq!(*device.writes.lock(), 10);
    for page in (0..19).step_by(2) {
        file.mapping()
            .with_folio(page as u64, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_under_writeback());
                assert_eq!(folio.is_dirty(), page == 8);
            })
            .unwrap();
    }
    *device.fail_data_write_at.lock() = None;
    assert_eq!(file.mapping().writepages(), Ok(()));
    assert_eq!(*device.writes.lock(), 11);
    // A successful retry cannot hide an unobserved synchronous writeback error.
    assert_eq!(file.fsync(true), Err(VfsError::Io));
    assert_eq!(file.fsync(true), Ok(()));
    let private = file.inode().downcast::<Ext4Inode>().unwrap();
    let state = ext4_private(&mount).unwrap().read();
    let mut output = vec![0; PAGE_SIZE_4K];
    for page in (0..19).step_by(2) {
        state
            .read_at(&private, (page * PAGE_SIZE_4K) as u64, &mut output)
            .unwrap();
        assert!(output.iter().all(|&byte| byte == 0x5a));
    }
}

#[def_test]
fn synchronous_writeback_settles_mapped_and_unwritten_batches_in_one_pass() {
    for data_only in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(5);
        let private = file.inode().downcast::<Ext4Inode>().unwrap();
        {
            let mut state = ext4_private(&mount).unwrap().write(&mount);
            let physical = match state.map_blocks(&private, LogicalBlock::new(2)).unwrap() {
                BlockMapping::Mapped { physical, .. } => physical,
                _ => panic!("fixture has initialized data"),
            };
            let journal = state.metadata_journal().unwrap();
            let mut handle = journal.begin(crate::jbd2::JournalCredits::new(32)).unwrap();
            state
                .remove_extent_range(
                    &private,
                    LogicalBlock::new(2),
                    crate::BlockCount::new(1),
                    &mut handle,
                )
                .unwrap();
            state
                .insert_extent_mapping(
                    &private,
                    LogicalBlock::new(2),
                    physical,
                    crate::BlockCount::new(1),
                    crate::extent::ExtentMappingState::Unwritten,
                    &mut handle,
                )
                .unwrap();
            state.complete_metadata_mutation(handle, Ok(())).unwrap();
            state.sync_filesystem().unwrap();
        }
        // Three disjoint batches switch from caller completion to a worker and back.
        for page in [0, 2, 4] {
            file.write_from(
                &vec![0x6c; PAGE_SIZE_4K],
                &mut ((page * PAGE_SIZE_4K) as u64),
            )
            .unwrap();
        }
        file.fsync(data_only).unwrap();
        assert_eq!(*device.writes.lock(), 3);
        let state = ext4_private(&mount).unwrap().read();
        assert!(matches!(
            state.map_blocks(&private, LogicalBlock::new(2)).unwrap(),
            BlockMapping::Mapped { .. }
        ));
        let mut output = vec![0; PAGE_SIZE_4K];
        for page in [0, 2, 4] {
            file.mapping()
                .with_folio(page as u64, |folio| {
                    let folio = folio.unwrap();
                    assert!(!folio.is_under_writeback());
                    assert!(!folio.is_dirty());
                })
                .unwrap();
            state
                .read_at(&private, (page * PAGE_SIZE_4K) as u64, &mut output)
                .unwrap();
            assert!(output.iter().all(|&byte| byte == 0x6c));
        }
    }
}

#[def_test]
fn mapped_writepages_keeps_prealloc_until_last_writable_close() {
    let (mut filesystem, _storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS + 64, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 8083, 341, true);
    let (_mount, file) = read_tests::cached_file(
        filesystem,
        private,
        SuperBlockFlags::empty(),
        OpenFlags::READ_WRITE,
    );
    let input = vec![0x37; PAGE_SIZE_4K * 4];
    file.write_from(&input, &mut 0).unwrap();
    file.mapping().writepages().unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    // Allocation preparation creates a space owned by the resident inode.
    assert!(!inode.prealloc_map().is_empty());
    file.write_from(&input, &mut 0).unwrap();
    file.mapping().writepages().unwrap();
    assert!(!inode.prealloc_map().is_empty());
    drop(file);
    assert!(inode.prealloc_map().is_empty());
}

#[def_test]
fn mapped_writeback_preserves_timestamp_changed_during_data_io() {
    for mode in [kvfs::WritebackSyncMode::All, kvfs::WritebackSyncMode::None] {
        for data_only in [false, true] {
            let (mount, file, device) = overwrite_fixture_with_pages(1);
            file.write_from(&vec![0x5e; PAGE_SIZE_4K], &mut 0).unwrap();
            let inode = file.inode().downcast::<Ext4Inode>().unwrap();
            let timestamp = Ext4Timestamp::new(123456, 0);
            *device.timestamp_update.lock() = Some((Arc::downgrade(&inode), timestamp));
            let mut control = kvfs::WritebackControl::all().with_sync_mode(mode);
            file.mapping().writepages_control(&mut control).unwrap();
            inode.writeback_io().drain();
            assert!(device.timestamp_update.lock().is_none());
            assert_eq!(inode.mtime(), timestamp);
            assert_eq!(inode.ctime(), timestamp);
            file.fsync(data_only).unwrap();
            let state = ext4_private(&mount).unwrap().read();
            let raw = state.raw_inode(inode.number()).unwrap();
            assert_eq!(raw.mtime(), 123456);
            assert_eq!(raw.ctime(), 123456);
        }
    }
}

fn set_old_write_timestamps(mount: &SuperBlock, file: &VfsFile) {
    let timestamp = SystemTime::from_unix_parts(1, 0).unwrap();
    file.inode()
        .update_metadata(
            &mount.root_dir(),
            MetadataUpdate {
                mtime: Some(timestamp),
                ctime: Some(timestamp),
                ..Default::default()
            },
        )
        .unwrap();
}

fn assert_write_timestamps_persisted(
    mount: &SuperBlock,
    file: &VfsFile,
    before: SystemTime,
    data_only: bool,
) {
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let written = inode.mtime();
    assert_ne!(written, Ext4Timestamp::new(1, 0));
    assert_eq!(inode.ctime(), written);
    let timestamp = ext4_timestamp_to_system_time(written);
    assert!(timestamp >= before);
    assert!(timestamp <= file.inode().current_time());
    file.fsync(data_only).unwrap();
    assert_eq!(inode.mtime(), written);
    assert_eq!(inode.ctime(), written);
    // fdatasync may omit timestamps; full fsync must persist them even after
    // data-only writeback has already cleaned every folio.
    file.fsync(false).unwrap();
    let state = ext4_private(mount).unwrap().read();
    let reloaded = state.load_inode_private(inode.number()).unwrap();
    assert_eq!(reloaded.mtime(), written);
    assert_eq!(reloaded.ctime(), written);
}

#[def_test]
fn buffered_writes_update_timestamps_before_writeback() {
    // Run with and without `times`: buffered writes retain the VFS policy in
    // both configurations, including updates of existing on-disk timestamps.
    for data_only in [false, true] {
        // New allocation, mapped overwrite, and append all use the write entry.
        for (pages, mut offset) in [(0, 0), (1, 0), (1, PAGE_SIZE_4K as u64)] {
            let (mount, file, _device) = overwrite_fixture_with_pages(pages);
            set_old_write_timestamps(&mount, &file);
            file.fsync(false).unwrap();
            let inode = file.inode().downcast::<Ext4Inode>().unwrap();
            let stored = ext4_private(&mount)
                .unwrap()
                .read()
                .load_inode_private(inode.number())
                .unwrap();
            assert_eq!(stored.mtime(), Ext4Timestamp::new(1, 0));
            assert_eq!(stored.ctime(), Ext4Timestamp::new(1, 0));
            assert_eq!(file.write_from(&[], &mut 0), Ok(0));
            assert!(file.write_from(b"x", &mut (i64::MAX as u64)).is_err());
            assert_eq!(inode.mtime(), Ext4Timestamp::new(1, 0));
            assert_eq!(inode.ctime(), Ext4Timestamp::new(1, 0));
            let before = file.inode().current_time();
            assert_eq!(file.write_from(b"written", &mut offset), Ok(7));
            assert_write_timestamps_persisted(&mount, &file, before, data_only);
        }
    }
}

#[def_test]
fn shared_writable_fault_updates_timestamps_before_writeback() {
    for data_only in [false, true] {
        let (mount, file, _device) = overwrite_fixture_with_pages(1);
        set_old_write_timestamps(&mount, &file);
        let before = file.inode().current_time();
        file.page_mkwrite(0, |folio| {
            folio.data()[0] = 0x5b;
            Ok(())
        })
        .unwrap();
        assert_write_timestamps_persisted(&mount, &file, before, data_only);
    }
}

#[def_test]
fn buffered_timestamp_updates_do_not_admit_a_journal_transaction() {
    let (mount, file, device) = overwrite_fixture_with_pages(1);
    set_old_write_timestamps(&mount, &file);
    file.fsync(false).unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let flushes = device.storage.flush_count();
    for byte in 0..32 {
        file.write_from(&[byte], &mut 0).unwrap();
    }
    assert!(inode.has_dirty_timestamps());
    let state = ext4_private(&mount).unwrap().read();
    assert!(
        state
            .journal
            .as_ref()
            .unwrap()
            .running_transaction()
            .unwrap()
            .is_none()
    );
    assert_eq!(device.storage.flush_count(), flushes);
    drop(state);
    file.fsync(true).unwrap();
    assert!(inode.has_dirty_timestamps());
    file.fsync(false).unwrap();
    assert!(!inode.has_dirty_timestamps());
    let reloaded = ext4_private(&mount)
        .unwrap()
        .read()
        .load_inode_private(inode.number())
        .unwrap();
    assert_eq!(reloaded.mtime(), inode.mtime());
    assert_eq!(reloaded.ctime(), inode.ctime());
}

#[def_test]
fn automatic_write_timestamps_reject_aborted_journal_before_publication() {
    let (mount, file, _device) = overwrite_fixture_with_pages(1);
    file.fsync(false).unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let before = inode.times();
    let error = Ext4Error::Device(DriverError::Io);
    assert_eq!(
        ext4_private(&mount)
            .unwrap()
            .read()
            .fail_journal_operation(error),
        error
    );
    let dentry = Dentry::new_file_from_inode(file.inode().clone(), None, String::from("timestamp"));
    let mut offset = 0;
    assert_eq!(
        file.write_from(b"rejected", &mut offset),
        Err(VfsError::ReadOnlyFilesystem)
    );
    assert_eq!(offset, 0);
    assert_eq!(inode.times(), before);
    file.mapping()
        .with_folio(0, |folio| {
            if let Some(folio) = folio {
                assert!(!folio.is_dirty());
                assert!(folio.data().iter().all(|byte| *byte == 0x41));
            }
        })
        .unwrap();
    assert!(
        inode
            .update_time(
                &kvfs::MountIdmap,
                &dentry,
                file.inode(),
                SystemTime::from_unix_seconds(789),
                InodeUpdateTime::ChangeAndModification,
            )
            .is_err()
    );
    assert_eq!(inode.times(), before);
    assert!(!inode.has_dirty_timestamps());
}

#[def_test]
fn syncfs_persists_timestamps_after_data_only_writeback() {
    let (mount, file, _device) = overwrite_fixture_with_pages(1);
    set_old_write_timestamps(&mount, &file);
    file.write_from(b"written", &mut 0).unwrap();
    file.fsync(true).unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    assert!(inode.has_dirty_timestamps());
    mount.sync_fs().unwrap();
    assert!(!inode.has_dirty_timestamps());
    let reloaded = ext4_private(&mount)
        .unwrap()
        .read()
        .load_inode_private(inode.number())
        .unwrap();
    assert_eq!(reloaded.mtime(), inode.mtime());
    assert_eq!(reloaded.ctime(), inode.ctime());
}

#[def_test]
fn timestamp_publication_preserves_concurrent_update_and_failed_staging() {
    let (mount, file, _device) = overwrite_fixture_with_pages(1);
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let older = Ext4Timestamp::new(123, 0);
    let newer = Ext4Timestamp::new(456, 0);
    assert!(inode.update_regular_write_timestamps(older));
    let mut state = ext4_private(&mount).unwrap().write(&mount);
    let journal = state.metadata_journal().unwrap();
    let mut handle = journal.begin(crate::jbd2::JournalCredits::new(2)).unwrap();
    state
        .update_dirty_inode_metadata(&inode, &mut handle, |_, _| {
            assert!(inode.update_regular_write_timestamps(newer));
            Ok(())
        })
        .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    assert_eq!(inode.mtime(), newer);
    assert_eq!(inode.ctime(), newer);
    assert!(inode.has_dirty_timestamps());
    let mut handle = journal.begin(crate::jbd2::JournalCredits::new(2)).unwrap();
    assert_eq!(
        state.update_dirty_inode_metadata(&inode, &mut handle, |_, _| {
            Err(Ext4Error::Device(DriverError::Io))
        }),
        Err(Ext4Error::Device(DriverError::Io))
    );
    handle.stop().unwrap();
    assert!(inode.has_dirty_timestamps());
    state.write_inode_timestamps(&inode).unwrap();
    state.sync_filesystem().unwrap();
    let reloaded = state.load_inode_private(inode.number()).unwrap();
    assert_eq!(reloaded.mtime(), newer);
    assert_eq!(reloaded.ctime(), newer);
    assert!(!inode.has_dirty_timestamps());
}

#[def_test]
fn linked_inode_eviction_stages_metadata_after_data_only_sync() {
    let (mount, file, _device) = overwrite_fixture_with_pages(1);
    set_old_write_timestamps(&mount, &file);
    file.write_from(b"written", &mut 0).unwrap();
    file.fsync(true).unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let modified = inode.mtime();
    let changed = inode.ctime();
    assert!(inode.has_dirty_timestamps());
    // Exercise the filesystem's teardown hook with no dirty data left.
    EXT4_SUPER_OPERATIONS.evict_inode(file.inode()).unwrap();
    assert!(!inode.has_dirty_timestamps());
    let mut state = ext4_private(&mount).unwrap().write(&mount);
    state.sync_filesystem().unwrap();
    let reloaded = state.load_inode_private(inode.number()).unwrap();
    assert_eq!(reloaded.mtime(), modified);
    assert_eq!(reloaded.ctime(), changed);
}

impl OverwriteDevice {
    fn finish_held(&self, fail: bool) {
        let requests = core::mem::take(&mut *self.delayed.lock());
        for bio in requests.into_iter().rev() {
            if fail {
                bio.end_io(Err(DriverError::Io));
            } else {
                self.storage.submit_bio(bio);
            }
        }
    }
}

#[def_test]
fn overlapping_fsync_callers_wait_for_data_and_keep_independent_errors() {
    for fail_data in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        let inode = file.inode().downcast::<Ext4Inode>().unwrap();
        let observer = file
            .alloc_clone(
                kvfs::FMode::READ | kvfs::FMode::WRITE,
                OpenFlags::READ_WRITE,
                inode.clone(),
            )
            .unwrap();
        let input = vec![0x69; PAGE_SIZE_4K];
        file.write_from(&input, &mut 0).unwrap();
        let written = inode.mtime();
        *device.hold_writes.lock() = true;
        let mut control =
            kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
        file.mapping().writepages_control(&mut control).unwrap();
        assert_eq!(device.delayed.lock().len(), 1);

        // Stop both callers at admission, after their error snapshots and
        // before the held I/O completes. This makes overlap deterministic.
        let submission = inode.lock_writeback_submission();
        let flushes = device.storage.flush_count();
        let mut waiters = Vec::new();
        for (caller, data_only) in [(file.clone(), true), (observer.clone(), false)] {
            waiters.push(ktask::spawn(move || {
                assert_eq!(
                    caller.fsync(data_only),
                    if fail_data { Err(VfsError::Io) } else { Ok(()) }
                );
            }));
        }
        for waiter in &waiters {
            while waiter.state() != ktask::TaskState::Blocked {
                assert_ne!(waiter.state(), ktask::TaskState::Exited);
                ktask::yield_now();
            }
        }
        assert_eq!(device.storage.flush_count(), flushes);
        device.finish_held(fail_data);
        inode.writeback_io().drain();
        *device.hold_writes.lock() = false;
        drop(submission);
        for waiter in waiters {
            assert_eq!(waiter.join(), 0);
        }
        if !fail_data {
            assert!(!inode.has_dirty_timestamps());
        }
        // Each fd has consumed its own failure. A successful retry must also
        // fulfill full-sync metadata requirements after data-only writeback.
        assert_eq!(file.fsync(true), Ok(()));
        assert_eq!(observer.fsync(false), Ok(()));
        let state = ext4_private(&mount).unwrap().read();
        let reloaded = state.load_inode_private(inode.number()).unwrap();
        assert_eq!(reloaded.mtime(), written);
        assert_eq!(reloaded.ctime(), written);
        let mut output = vec![0; input.len()];
        state.read_at(&inode, 0, &mut output).unwrap();
        assert_eq!(output, input);
    }
}

#[def_test]
fn async_overwrite_returns_with_bios_pending_and_bounds_admission() {
    let (_mount, file, device) = overwrite_fixture_with_pages(9);
    for page in [0, 2, 4, 6, 8] {
        file.write_from(
            &vec![0x76; PAGE_SIZE_4K],
            &mut ((page * PAGE_SIZE_4K) as u64),
        )
        .unwrap();
    }
    *device.hold_writes.lock() = true;
    let mut control = kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
    file.mapping().writepages_control(&mut control).unwrap();
    assert_eq!(device.delayed.lock().len(), 4);
    assert_eq!(*device.writes.lock(), 4);
    assert_eq!(control.pages_skipped(), 1);
    for page in [0, 2, 4, 6, 8] {
        file.mapping()
            .with_folio(page, |folio| {
                let folio = folio.unwrap();
                assert_eq!(folio.is_under_writeback(), page != 8);
                assert_eq!(folio.is_dirty(), page == 8);
            })
            .unwrap();
    }
    // A second opportunistic pass must not wait for a full queue.
    file.mapping().writepages_control(&mut control).unwrap();
    assert_eq!(*device.writes.lock(), 4);
    assert_eq!(control.pages_skipped(), 2);
    assert_eq!(control.nr_to_write(), usize::MAX - 6);
    device.finish_held(false);
    file.inode()
        .private::<Ext4Inode>()
        .unwrap()
        .writeback_io()
        .drain();
    *device.hold_writes.lock() = false;
    file.fsync(true).unwrap();
    // Capacity throttling must not leave an error in either channel.
    file.sync_filesystem().unwrap();
    assert_eq!(*device.writes.lock(), 5);
    for page in [0, 2, 4, 6, 8] {
        file.mapping()
            .with_folio(page, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
    }
}

#[def_test]
fn writeback_reports_pending_io_failure_while_waiting_for_submission() {
    let (_mount, file, device) = overwrite_fixture_with_pages(1);
    let private = file.inode().downcast::<Ext4Inode>().unwrap();
    file.write_from(&vec![0x62; PAGE_SIZE_4K], &mut 0).unwrap();
    *device.hold_writes.lock() = true;
    let mut control = kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
    file.mapping().writepages_control(&mut control).unwrap();
    assert_eq!(device.delayed.lock().len(), 1);

    // Force the synchronous caller to wait at submission admission. Its error
    // snapshot must precede that wait, not just the later dirty-page scan.
    let submission = private.lock_writeback_submission();
    let waiter = {
        let file = file.clone();
        ktask::spawn(move || {
            assert_eq!(file.mapping().writepages(), Err(VfsError::Io));
        })
    };
    while waiter.state() != ktask::TaskState::Blocked {
        assert_ne!(waiter.state(), ktask::TaskState::Exited);
        ktask::yield_now();
    }
    device.finish_held(true);
    private.writeback_io().drain();
    *device.hold_writes.lock() = false;
    drop(submission);
    assert_eq!(waiter.join(), 0);
    assert_eq!(*device.writes.lock(), 2);
    file.mapping()
        .with_folio(0, |folio| {
            let folio = folio.unwrap();
            assert!(!folio.is_dirty());
            assert!(!folio.is_under_writeback());
        })
        .unwrap();
    assert_eq!(file.mapping().writepages(), Ok(()));
    assert_eq!(file.fsync(true), Err(VfsError::Io));
    assert_eq!(file.fsync(true), Ok(()));
    assert_eq!(file.sync_filesystem(), Err(VfsError::Io));
    assert_eq!(file.sync_filesystem(), Ok(()));
}

#[def_test]
fn repeated_syncfs_reports_new_failures_without_consuming_mapping_history() {
    let (mount, file, device) = overwrite_fixture_with_len(1024);
    let observer = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    // Only syncfs observes these failures. No file fsync may mark the mapping
    // error seen before both the dirty retry and the clean repeat succeed.
    for byte in [0x73, 0x74] {
        file.write_from(&vec![byte; 1024], &mut 0).unwrap();
        let writes = *device.writes.lock();
        *device.fail_data_reads.lock() = true;
        assert_eq!(file.sync_filesystem(), Err(VfsError::Io));
        assert_eq!(*device.writes.lock(), writes);
        file.mapping()
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
        assert!(
            !ext4_private(&mount)
                .unwrap()
                .read()
                .journal
                .as_ref()
                .unwrap()
                .is_aborted()
        );
        *device.fail_data_reads.lock() = false;
        assert_eq!(file.sync_filesystem(), Ok(()));
        assert_eq!(file.sync_filesystem(), Ok(()));
        assert_eq!(*device.writes.lock(), writes + 1);
        file.mapping()
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(!folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
    }
    let reopened = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    // The old observer still owns pending superblock history; a later open
    // skips that seen history but must inherit the still-unseen mapping error.
    assert_eq!(observer.sync_filesystem(), Err(VfsError::Io));
    assert_eq!(observer.sync_filesystem(), Ok(()));
    assert_eq!(reopened.sync_filesystem(), Ok(()));
    for file in [file, observer, reopened] {
        assert_eq!(file.fsync(true), Err(VfsError::Io));
        assert_eq!(file.fsync(true), Ok(()));
    }
}

#[def_test]
fn partial_block_preparation_failure_reaches_other_files_and_syncfs() {
    let (mount, file, device) = overwrite_fixture_with_len(1024);
    let observer = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    file.write_from(&vec![0x73; 1024], &mut 0).unwrap();
    let writes = *device.writes.lock();
    *device.fail_data_reads.lock() = true;
    assert_eq!(file.fsync(true), Err(VfsError::Io));
    assert_eq!(*device.writes.lock(), writes);
    file.mapping()
        .with_folio(0, |folio| {
            let folio = folio.unwrap();
            assert!(folio.is_dirty());
            assert!(!folio.is_under_writeback());
        })
        .unwrap();
    assert!(
        !ext4_private(&mount)
            .unwrap()
            .read()
            .journal
            .as_ref()
            .unwrap()
            .is_aborted()
    );
    *device.fail_data_reads.lock() = false;
    // The retry writes successfully, but this pre-existing observer still sees EIO.
    assert_eq!(observer.fsync(true), Err(VfsError::Io));
    assert_eq!(observer.fsync(true), Ok(()));
    assert_eq!(*device.writes.lock(), writes + 1);
    let reopened = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    drop(file);
    // Mapping observation does not consume the independent filesystem error.
    assert_eq!(reopened.fsync(true), Ok(()));
    assert_eq!(reopened.sync_filesystem(), Err(VfsError::Io));
    assert_eq!(reopened.sync_filesystem(), Ok(()));
}

#[def_test]
fn async_failure_survives_retry_and_is_seen_by_each_open_file_and_syncfs() {
    let (_mount, file, device) = overwrite_fixture_with_pages(1);
    let observer = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    file.write_from(&vec![0x67; PAGE_SIZE_4K], &mut 0).unwrap();
    *device.hold_writes.lock() = true;
    let mut control = kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
    file.mapping().writepages_control(&mut control).unwrap();
    device.finish_held(true);
    file.inode()
        .private::<Ext4Inode>()
        .unwrap()
        .writeback_io()
        .drain();
    *device.hold_writes.lock() = false;
    let reopened = file
        .alloc_clone(
            kvfs::FMode::READ | kvfs::FMode::WRITE,
            OpenFlags::READ_WRITE,
            file.inode().downcast::<Ext4Inode>().unwrap(),
        )
        .unwrap();
    // Retrying the dirty folio neither erases nor consumes an unseen error.
    assert_eq!(file.mapping().writepages(), Ok(()));
    assert_eq!(file.fsync(true), Err(VfsError::Io));
    assert_eq!(file.fsync(true), Ok(()));
    assert_eq!(observer.fsync(true), Err(VfsError::Io));
    assert_eq!(observer.fsync(true), Ok(()));
    assert_eq!(reopened.fsync(true), Err(VfsError::Io));
    assert_eq!(reopened.fsync(true), Ok(()));
    assert_eq!(file.sync_filesystem(), Err(VfsError::Io));
    assert_eq!(file.sync_filesystem(), Ok(()));
    assert_eq!(observer.sync_filesystem(), Err(VfsError::Io));
    assert_eq!(observer.sync_filesystem(), Ok(()));
}

#[def_test]
fn async_overwrite_finishes_before_truncate_or_last_writer_close() {
    for (truncate, pages) in [(true, 1), (false, 1), (true, 0), (false, 0)] {
        let (_mount, file, device) = overwrite_fixture_with_pages(pages);
        file.write_from(&vec![0x32; PAGE_SIZE_4K], &mut 0).unwrap();
        *device.hold_writes.lock() = true;
        let mut control =
            kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
        file.mapping().writepages_control(&mut control).unwrap();
        let inode = file.inode().clone();
        let waiter = ktask::spawn(move || {
            if truncate {
                file.truncate(0).unwrap();
            }
            drop(file);
        });
        while waiter.state() != ktask::TaskState::Blocked {
            assert_ne!(waiter.state(), ktask::TaskState::Exited);
            ktask::yield_now();
        }
        assert_eq!(inode.size(), PAGE_SIZE_4K as u64);
        assert_eq!(device.delayed.lock().len(), 1);
        // The completion worker must progress despite the waiter owning the
        // submission mutex (and truncate owning invalidate-exclusive protection).
        device.finish_held(false);
        assert_eq!(waiter.join(), 0);
        assert_eq!(inode.size(), if truncate { 0 } else { PAGE_SIZE_4K as u64 });
        assert_eq!(inode.write_count(), 0);
    }
}

#[def_test]
fn async_allocation_and_existing_unwritten_publish_only_after_data_completion() {
    for already_unwritten in [false, true] {
        for data_only in [false, true] {
            let (mount, file, device) = overwrite_fixture_with_pages(0);
            let private = file.inode().downcast::<Ext4Inode>().unwrap();
            let input = vec![0x8b; PAGE_SIZE_4K + 37];
            file.write_from(&input, &mut 0).unwrap();
            let ext4 = ext4_private(&mount).unwrap();
            if already_unwritten {
                // A previous preparation may persist even if submission failed.
                drop(
                    ext4.write(&mount)
                        .prepare_allocating_writeback(
                            &private,
                            0,
                            input.len(),
                            file.inode().size(),
                            Ext4SyncIntent::DataOnly,
                        )
                        .unwrap(),
                );
            }
            *device.hold_writes.lock() = true;
            let mut control =
                kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
            file.mapping().writepages_control(&mut control).unwrap();
            assert!(!device.delayed.lock().is_empty());
            assert_eq!(private.disk_size(), 0);
            assert!(!private.has_delalloc_reservations());
            for logical in [0, 1] {
                let physical = match ext4
                    .read()
                    .map_blocks(&private, LogicalBlock::new(logical))
                    .unwrap()
                {
                    BlockMapping::Unwritten { physical, .. } => physical,
                    _ => panic!("pending data must remain unwritten"),
                };
                crate::io::test_block_device(device.storage.clone())
                    .write_block(physical.get(), &vec![0xd3; PAGE_SIZE_4K])
                    .unwrap();
            }
            let mut output = vec![0xff; input.len()];
            ext4.read().read_at(&private, 0, &mut output).unwrap();
            assert!(output.iter().all(|byte| *byte == 0));
            // Unwritten allocation can commit while BIOs remain pending. A
            // reopened disk must retain the old size and hide stale data.
            ext4.write(&mount).sync_filesystem().unwrap();
            let reopened = Ext4SbInfo::open(
                crate::io::test_block_device(Arc::new(TestDevice::new(device.storage.bytes()))),
                false,
            )
            .unwrap();
            let disk_inode = reopened.load_inode_private(private.number()).unwrap();
            assert_eq!(disk_inode.disk_size(), 0);
            assert!(matches!(
                reopened
                    .map_blocks(&disk_inode, LogicalBlock::new(0))
                    .unwrap(),
                BlockMapping::Unwritten { .. }
            ));
            device.finish_held(false);
            private.writeback_io().drain();
            *device.hold_writes.lock() = false;
            file.fsync(data_only).unwrap();
            assert_eq!(private.disk_size(), input.len() as u64);
            assert!(!private.has_delalloc_reservations());
            assert_eq!(
                ext4.read().read_at(&private, 0, &mut output),
                Ok(input.len())
            );
            assert_eq!(output, input);
            let physical = match ext4
                .read()
                .map_blocks(&private, LogicalBlock::new(1))
                .unwrap()
            {
                BlockMapping::Mapped { physical, .. } => physical,
                _ => panic!("completed data must be initialized"),
            };
            let mut tail = vec![0xff; PAGE_SIZE_4K];
            crate::io::test_block_device(device.storage.clone())
                .read_block(physical.get(), &mut tail)
                .unwrap();
            assert!(tail[37..].iter().all(|byte| *byte == 0));
            // Append into the already initialized partial EOF block: preparation
            // must preserve its old bytes and still defer the new disk size.
            let suffix = [0x45; 19];
            file.write_from(&suffix, &mut (input.len() as u64)).unwrap();
            *device.hold_writes.lock() = true;
            file.mapping().writepages_control(&mut control).unwrap();
            assert_eq!(private.disk_size(), input.len() as u64);
            assert!(!device.delayed.lock().is_empty());
            device.finish_held(false);
            private.writeback_io().drain();
            *device.hold_writes.lock() = false;
            file.fsync(data_only).unwrap();
            let mut appended = vec![0; input.len() + suffix.len()];
            ext4.read().read_at(&private, 0, &mut appended).unwrap();
            assert_eq!(&appended[..input.len()], input.as_slice());
            assert_eq!(&appended[input.len()..], &suffix);
            assert_eq!(private.disk_size(), appended.len() as u64);
        }
    }
}

#[def_test]
fn failed_async_allocation_retries_existing_unwritten_blocks() {
    for fail_metadata in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(0);
        let private = file.inode().downcast::<Ext4Inode>().unwrap();
        let input = vec![0x71; PAGE_SIZE_4K];
        file.write_from(&input, &mut 0).unwrap();
        *device.hold_writes.lock() = true;
        let mut control =
            kvfs::WritebackControl::all().with_sync_mode(kvfs::WritebackSyncMode::None);
        file.mapping().writepages_control(&mut control).unwrap();
        let free_after_prepare = ext4_private(&mount).unwrap().read().free_blocks_count();
        if fail_metadata {
            let mut state = ext4_private(&mount).unwrap().write(&mount);
            state.sync_filesystem().unwrap();
            crate::journal::tests::override_mounted_transaction_credits(&state, 39);
        }
        device.finish_held(!fail_metadata);
        private.writeback_io().drain();
        assert_eq!(private.disk_size(), 0);
        assert!(matches!(
            ext4_private(&mount)
                .unwrap()
                .read()
                .map_blocks(&private, LogicalBlock::new(0))
                .unwrap(),
            BlockMapping::Unwritten { .. }
        ));
        file.mapping()
            .with_folio(0, |folio| {
                let folio = folio.unwrap();
                assert!(folio.is_dirty());
                assert!(!folio.is_under_writeback());
            })
            .unwrap();
        *device.hold_writes.lock() = false;
        if fail_metadata {
            crate::journal::tests::override_mounted_transaction_credits(
                &ext4_private(&mount).unwrap().write(&mount),
                341,
            );
        }
        assert_eq!(file.mapping().writepages(), Ok(()));
        assert_eq!(
            ext4_private(&mount).unwrap().read().free_blocks_count(),
            free_after_prepare
        );
        assert_eq!(private.disk_size(), PAGE_SIZE_4K as u64);
        assert!(!private.has_delalloc_reservations());
        assert_eq!(
            file.fsync(true),
            Err(if fail_metadata {
                VfsError::StorageFull
            } else {
                VfsError::Io
            })
        );
        assert_eq!(file.fsync(true), Ok(()));
    }
}
