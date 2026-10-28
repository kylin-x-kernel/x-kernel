// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mount-unlocked mapped writeback and PageCache failure contracts.

use alloc::{sync::Weak, vec};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError};
use kvfs::OpenFlags;
use unittest::def_test;

use super::*;
use crate::{
    io::FilesystemDevice,
    superblock::test_fixtures::{
        TEST_FREE_BLOCKS, TestDevice, allocate_checkpointed_regular_inode,
        install_credit_limited_journal, journal_allocator_test_filesystem,
    },
    sync::Mutex,
};

struct OverwriteDevice {
    storage: Arc<TestDevice>,
    mount: Mutex<Weak<SuperBlock>>,
    writes: Mutex<usize>,
    fail_writes: Mutex<bool>,
    delayed: Mutex<Vec<block::Bio>>,
    delay_writes: Mutex<bool>,
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
            if *self.fail_writes.lock() {
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
    let (mut filesystem, storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS + 64, 0b0011_1111);
    let private = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_credit_limited_journal(&mut filesystem, 8082, 341, true);
    private.set_size((PAGE_SIZE_4K * pages) as u64);
    filesystem
        .writeback_ordered_at(
            &private,
            0,
            &vec![0x41; PAGE_SIZE_4K * pages],
            (PAGE_SIZE_4K * pages) as u64,
            Ext4SyncIntent::DataOnly,
            || false,
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    let data_blocks = (0..pages)
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
    let device = Arc::new(OverwriteDevice {
        storage,
        mount: Mutex::new(Weak::new()),
        writes: Mutex::new(0),
        fail_writes: Mutex::new(false),
        delayed: Mutex::new(Vec::new()),
        delay_writes: Mutex::new(false),
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
fn mapped_writepages_releases_mount_protection_and_preserves_sync_barriers() {
    for data_only in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        let input = vec![0x7b; PAGE_SIZE_4K];
        assert_eq!(file.write_from(&input, &mut 0), Ok(input.len()));
        let flushes = device.storage.flush_count();
        assert_eq!(file.mapping().writepages(data_only), Ok(()));
        assert_eq!(*device.writes.lock(), 1);
        assert_eq!(device.storage.flush_count(), flushes);
        file.mapping().with_folio(0, |folio| {
            let folio = folio.unwrap();
            assert!(!folio.is_dirty());
            assert!(!folio.is_under_writeback());
        });
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
        assert_eq!(file.fsync(data_only), Ok(()));
        // Both sync intents commit the write-time timestamp transaction. Its
        // first commit also activates the recovery flag and journal header,
        // each with its own flush. Full writeback additionally registers an
        // ordered-data dependency, while data-only keeps its mapped fast path.
        assert_eq!(
            device.storage.flush_count() - flushes,
            if data_only { 3 } else { 4 }
        );
        if !data_only {
            assert_eq!(file.write_from(&input, &mut 0), Ok(input.len()));
            let flushes = device.storage.flush_count();
            assert_eq!(file.mapping().writepages(false), Ok(()));
            assert_eq!(device.storage.flush_count(), flushes);
            assert_eq!(file.fsync(false), Ok(()));
            // An already-active journal retains exactly the ordered-data and
            // commit barriers, without a per-range data-phase flush.
            assert_eq!(device.storage.flush_count() - flushes, 2);
        }
    }
}

#[def_test]
fn mapped_writepages_data_failure_preserves_dirty_folio_and_aborts_full_metadata() {
    let (mount, file, device) = overwrite_fixture_with_pages(1);
    assert_eq!(
        file.write_from(&vec![0x9c; PAGE_SIZE_4K], &mut 0),
        Ok(PAGE_SIZE_4K)
    );
    *device.fail_writes.lock() = true;
    assert_eq!(file.mapping().writepages(false), Err(VfsError::Io));
    file.mapping().with_folio(0, |folio| {
        let folio = folio.unwrap();
        assert!(folio.is_dirty());
        assert!(!folio.is_under_writeback());
    });
    let ext4 = ext4_private(&mount).unwrap().read();
    assert!(ext4.journal.as_ref().unwrap().is_aborted());
    assert!(mount.flags().contains(SuperBlockFlags::RDONLY));
    assert_eq!(ext4.pending_checkpoint_count(), 0);
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
    assert_eq!(file.mapping().writepages(true), Err(VfsError::Io));
    assert_eq!(*device.writes.lock(), 4);
    assert!(device.delayed.lock().is_empty());
    for page in [0, 2, 4, 6] {
        file.mapping().with_folio(page, |folio| {
            let folio = folio.unwrap();
            assert!(!folio.is_under_writeback());
            assert_eq!(folio.is_dirty(), page == 4);
        });
    }
    *device.delay_writes.lock() = false;
    assert_eq!(file.mapping().writepages(true), Ok(()));
    assert_eq!(*device.writes.lock(), 5);
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
    file.mapping().writepages(true).unwrap();
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    // The allocating fallback creates a space owned by the resident inode.
    assert!(!inode.prealloc_map().is_empty());
    for data_only in [true, false] {
        file.write_from(&input, &mut 0).unwrap();
        file.mapping().writepages(data_only).unwrap();
        assert!(!inode.prealloc_map().is_empty());
    }
    drop(file);
    assert!(inode.prealloc_map().is_empty());
}

#[def_test]
fn mapped_writeback_preserves_timestamp_changed_during_data_io() {
    for data_only in [false, true] {
        let (mount, file, device) = overwrite_fixture_with_pages(1);
        file.write_from(&vec![0x5e; PAGE_SIZE_4K], &mut 0).unwrap();
        let inode = file.inode().downcast::<Ext4Inode>().unwrap();
        let timestamp = Ext4Timestamp::new(123456, 0);
        *device.timestamp_update.lock() = Some((Arc::downgrade(&inode), timestamp));
        file.mapping().writepages(data_only).unwrap();
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
    let state = ext4_private(mount).unwrap().read();
    let reloaded = state.load_inode_private(inode.number()).unwrap();
    assert_eq!(reloaded.mtime(), written);
    assert_eq!(reloaded.ctime(), written);
}

#[def_test]
fn buffered_writes_update_timestamps_before_writeback() {
    for data_only in [false, true] {
        // New allocation, mapped overwrite, and append all use the write entry.
        for (pages, mut offset) in [(0, 0), (1, 0), (1, PAGE_SIZE_4K as u64)] {
            let (mount, file, _device) = overwrite_fixture_with_pages(pages);
            set_old_write_timestamps(&mount, &file);
            assert_eq!(file.write_from(&[], &mut 0), Ok(0));
            assert!(file.write_from(b"x", &mut (i64::MAX as u64)).is_err());
            let inode = file.inode().downcast::<Ext4Inode>().unwrap();
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
