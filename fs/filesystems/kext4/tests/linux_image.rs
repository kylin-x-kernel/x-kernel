// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};
use kext4::{
    BlockMapping, DirectoryFileType, Ext4DirEntryRef, Ext4DirPos, Ext4DirSink, Ext4Error,
    Ext4Filesystem, Ext4Inode, Ext4Result, Ext4StatFsMode, Ext4Timestamp, Ext4XattrNamespace,
    FilesystemBlock, InodeKind, InodeNumber, LogicalBlock, UnsupportedKind,
};

const DEVICE_BLOCK_SIZE: usize = 512;

struct ImageDevice {
    bytes: Box<[u8]>,
}

impl ImageDevice {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: bytes.into_boxed_slice(),
        }
    }
}

struct WritableImageDevice {
    bytes: Mutex<Vec<u8>>,
}

struct WriteThroughFaultyImageDevice {
    bytes: Mutex<Vec<u8>>,
    fail_write_at: Option<usize>,
    write_count: Mutex<usize>,
}

struct FaultyBufferedImageDevice {
    committed: Mutex<Vec<u8>>,
    pending: Mutex<Vec<(usize, Vec<u8>)>>,
    fail_write_at: Option<usize>,
    fail_flush_at: Option<usize>,
    write_count: Mutex<usize>,
    flush_count: Mutex<usize>,
}

enum XattrFault {
    Write {
        device: Arc<FaultyBufferedImageDevice>,
    },
    Flush {
        device: Arc<FaultyBufferedImageDevice>,
    },
}

impl WritableImageDevice {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Mutex::new(bytes),
        }
    }

    fn committed_bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl WriteThroughFaultyImageDevice {
    fn fail_write_at(bytes: Vec<u8>, write: usize) -> Self {
        Self {
            bytes: Mutex::new(bytes),
            fail_write_at: Some(write),
            write_count: Mutex::new(0),
        }
    }

    fn committed_bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl FaultyBufferedImageDevice {
    fn fail_write_at(bytes: Vec<u8>, write: usize) -> Self {
        Self::new(bytes, Some(write), None)
    }

    fn fail_flush_at(bytes: Vec<u8>, flush: usize) -> Self {
        Self::new(bytes, None, Some(flush))
    }

    fn new(bytes: Vec<u8>, fail_write_at: Option<usize>, fail_flush_at: Option<usize>) -> Self {
        Self {
            committed: Mutex::new(bytes),
            pending: Mutex::new(Vec::new()),
            fail_write_at,
            fail_flush_at,
            write_count: Mutex::new(0),
            flush_count: Mutex::new(0),
        }
    }

    fn committed_bytes(&self) -> Vec<u8> {
        self.committed.lock().unwrap().clone()
    }
}

impl XattrFault {
    fn device(&self) -> Arc<FaultyBufferedImageDevice> {
        match self {
            Self::Write { device } | Self::Flush { device } => device.clone(),
        }
    }

    fn committed_bytes(&self) -> Vec<u8> {
        match self {
            Self::Write { device } | Self::Flush { device } => device.committed_bytes(),
        }
    }
}

impl Device for WritableImageDevice {
    fn name(&self) -> &str {
        "kext4-writable-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for WritableImageDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().unwrap().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(DEVICE_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.bytes
                .lock()
                .unwrap()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(DEVICE_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        self.bytes
            .lock()
            .unwrap()
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl Device for WriteThroughFaultyImageDevice {
    fn name(&self) -> &str {
        "kext4-write-through-faulty-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for WriteThroughFaultyImageDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().unwrap().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = block_start(block_id)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.bytes
                .lock()
                .unwrap()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let mut write_count = self.write_count.lock().unwrap();
        *write_count += 1;
        if self.fail_write_at == Some(*write_count) {
            return Err(DriverError::Io);
        }

        let start = block_start(block_id)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        self.bytes
            .lock()
            .unwrap()
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl Device for FaultyBufferedImageDevice {
    fn name(&self) -> &str {
        "kext4-faulty-buffered-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for FaultyBufferedImageDevice {
    fn num_blocks(&self) -> u64 {
        (self.committed.lock().unwrap().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = block_start(block_id)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.committed
                .lock()
                .unwrap()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );

        for (pending_start, pending_bytes) in self.pending.lock().unwrap().iter() {
            let pending_end = pending_start
                .checked_add(pending_bytes.len())
                .ok_or(DriverError::InvalidInput)?;
            let overlap_start = start.max(*pending_start);
            let overlap_end = end.min(pending_end);
            if overlap_start < overlap_end {
                let output_start = overlap_start - start;
                let pending_offset = overlap_start - pending_start;
                let len = overlap_end - overlap_start;
                output[output_start..output_start + len]
                    .copy_from_slice(&pending_bytes[pending_offset..pending_offset + len]);
            }
        }
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let mut write_count = self.write_count.lock().unwrap();
        *write_count += 1;
        if self.fail_write_at == Some(*write_count) {
            return Err(DriverError::Io);
        }

        let start = block_start(block_id)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        if end > self.committed.lock().unwrap().len() {
            return Err(DriverError::InvalidInput);
        }
        self.pending.lock().unwrap().push((start, input.to_vec()));
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        let mut flush_count = self.flush_count.lock().unwrap();
        *flush_count += 1;
        if self.fail_flush_at == Some(*flush_count) {
            self.pending.lock().unwrap().clear();
            return Err(DriverError::Io);
        }

        let mut committed = self.committed.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        for (start, bytes) in pending.drain(..) {
            let end = start
                .checked_add(bytes.len())
                .ok_or(DriverError::InvalidInput)?;
            committed
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?
                .copy_from_slice(&bytes);
        }
        Ok(())
    }
}

impl Device for ImageDevice {
    fn name(&self) -> &str {
        "kext4-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

fn block_start(block_id: u64) -> Result<usize, DriverError> {
    usize::try_from(block_id)
        .map_err(|_| DriverError::InvalidInput)?
        .checked_mul(DEVICE_BLOCK_SIZE)
        .ok_or(DriverError::InvalidInput)
}

fn set_large_xattr_on_file(
    filesystem: &mut Ext4Filesystem,
    large_value: &[u8],
) -> Ext4Result<Ext4Inode> {
    let root = filesystem.root_inode()?;
    let entry = filesystem.lookup(&root, "file")?.ok_or(Ext4Error::Corrupt(
        kext4::CorruptKind::InvalidDirectoryEntry,
    ))?;
    let inode = filesystem.load_inode_private(entry.inode())?;
    filesystem.set_xattr(
        &inode,
        Ext4XattrNamespace::User,
        b"large",
        large_value,
        Ext4Timestamp::new(1_720_000_020, 0),
    )?;
    Ok(inode)
}

fn assert_large_xattr_absent(bytes: &[u8]) {
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.to_vec())))
        .expect("mount committed xattr image");
    let root = filesystem.root_inode().expect("read committed root");
    let entry = filesystem
        .lookup(&root, "file")
        .expect("lookup committed xattr file")
        .expect("committed xattr file exists");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read committed xattr inode");
    assert!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
            .expect("read absent large xattr")
            .is_none()
    );
}

impl BlockDeviceOperations for ImageDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(DEVICE_BLOCK_SIZE)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.bytes
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, _block_id: u64, _input: &[u8]) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

#[test]
fn mounts_linux_metadata_csum_image() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("valid");
    create_image(&mke2fs, &image);

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.clone())))
        .expect("mount generated Linux ext4 image");
    assert_eq!(filesystem.layout().block_size(), 4096);
    assert_eq!(
        filesystem.groups().len(),
        filesystem.layout().group_count() as usize
    );
    let journal = filesystem
        .journal_status()
        .expect("internal journal is present");
    assert_eq!(journal.block_size(), filesystem.layout().block_size());
    assert!(!journal.has_nonzero_log_start());

    let mut block = vec![0; filesystem.layout().block_size() as usize];
    filesystem
        .read_blocks(FilesystemBlock::new(0), 1, &mut block)
        .expect("read first filesystem block");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn mounts_linux_image_without_a_journal() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("no-journal");
    create_image_with_journal(&mke2fs, &image, false);

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount generated Linux ext4 image without journal");
    assert!(filesystem.journal_status().is_none());
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_linux_image_marked_as_needing_recovery() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("needs-recovery");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    mark_image_needs_recovery(&mut bytes);

    let error = match Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("filesystem requiring recovery unexpectedly mounted"),
        Err(error) => error,
    };
    assert_eq!(error, Ext4Error::NeedsRecovery);
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn recovers_linux_image_marked_as_needing_recovery() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("recover");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    mark_image_needs_recovery(&mut bytes);
    let device = Arc::new(WritableImageDevice::new(bytes));

    let report = Ext4Filesystem::recover(device.clone())
        .expect("recover generated Linux ext4 image")
        .expect("recovery was required");
    assert_eq!(report.update_count(), 0);

    let filesystem = Ext4Filesystem::mount(device).expect("mount recovered Linux ext4 image");
    assert!(!filesystem.superblock().features().needs_recovery());
    let journal = filesystem.journal_status().expect("internal journal");
    assert_eq!(journal.start_block(), None);
    assert_eq!(journal.sequence(), report.next_sequence());
    assert_eq!(journal.head(), report.head());
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn recovers_e2fsprogs_written_journal_transaction() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture = journaled_update_image_bytes(&mke2fs, &debugfs, "recover-real-journal", 3, true);
    let device = Arc::new(WritableImageDevice::new(fixture.bytes.clone()));
    let report = Ext4Filesystem::recover(device.clone())
        .expect("recover e2fsprogs journal transaction")
        .expect("recovery was required");

    assert_eq!(report.update_count(), fixture.targets.len());
    assert_eq!(report.revoke_hit_count(), 0);

    let filesystem =
        Ext4Filesystem::mount(device).expect("mount recovered e2fsprogs journal image");
    let journal = filesystem.journal_status().expect("internal journal");
    assert_eq!(journal.start_block(), None);
    assert_eq!(journal.sequence(), report.next_sequence());
    assert_eq!(journal.head(), report.head());
    assert_filesystem_blocks(&filesystem, &fixture.targets, &fixture.replacements);
}

#[test]
fn recovers_e2fsprogs_descriptor_layout_matrix() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");

    for (journal_version, has_64bit) in [(2, false), (2, true), (3, false), (3, true)] {
        let label = format!("recover-journal-v{journal_version}-{has_64bit}");
        let fixture =
            journaled_update_image_bytes(&mke2fs, &debugfs, &label, journal_version, has_64bit);
        let device = Arc::new(WritableImageDevice::new(fixture.bytes.clone()));
        let report = Ext4Filesystem::recover(device.clone())
            .unwrap_or_else(|error| {
                panic!("recover e2fsprogs journal v{journal_version} 64bit={has_64bit}: {error:?}")
            })
            .expect("recovery was required");

        assert_eq!(report.update_count(), fixture.targets.len());
        let filesystem =
            Ext4Filesystem::mount(device).expect("mount recovered descriptor-layout image");
        assert_filesystem_blocks(&filesystem, &fixture.targets, &fixture.replacements);
    }
}

#[test]
fn recovery_partial_replay_write_failure_keeps_ext4_recovery_feature() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture =
        journaled_update_image_bytes(&mke2fs, &debugfs, "recover-partial-replay-fail", 3, true);
    let fail_write = fixture.device_writes_per_filesystem_block + 1;
    let device = Arc::new(WriteThroughFaultyImageDevice::fail_write_at(
        fixture.bytes.clone(),
        fail_write,
    ));

    let error = Ext4Filesystem::recover(device.clone()).expect_err("partial replay write fails");

    assert!(matches!(error, Ext4Error::Device(DriverError::Io)));
    let committed = device.committed_bytes();
    assert!(image_needs_recovery(&committed));
    assert_image_block(
        &committed,
        fixture.targets[0],
        fixture.block_size,
        &fixture.replacements[0],
    );
    assert_image_block(
        &committed,
        fixture.targets[1],
        fixture.block_size,
        &fixture.originals[1],
    );
}

#[test]
fn recovery_replay_flush_failure_keeps_ext4_recovery_feature() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture =
        journaled_update_image_bytes(&mke2fs, &debugfs, "recover-replay-flush-fail", 3, true);
    let device = Arc::new(FaultyBufferedImageDevice::fail_flush_at(
        fixture.bytes.clone(),
        1,
    ));

    let error = Ext4Filesystem::recover(device.clone()).expect_err("recovery flush fails");

    assert!(matches!(error, Ext4Error::Device(DriverError::Io)));
    let committed = device.committed_bytes();
    assert!(image_needs_recovery(&committed));
    assert_image_blocks(
        &committed,
        &fixture.targets,
        fixture.block_size,
        &fixture.originals,
    );
}

#[test]
fn recovery_journal_empty_write_failure_keeps_ext4_recovery_feature() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture =
        journaled_update_image_bytes(&mke2fs, &debugfs, "recover-journal-write-fail", 3, true);
    let replay_writes = fixture.targets.len() * fixture.device_writes_per_filesystem_block;
    let device = Arc::new(FaultyBufferedImageDevice::fail_write_at(
        fixture.bytes.clone(),
        replay_writes + 1,
    ));

    let error = Ext4Filesystem::recover(device.clone()).expect_err("journal write fails");

    assert!(matches!(error, Ext4Error::Device(DriverError::Io)));
    let committed = device.committed_bytes();
    assert!(image_needs_recovery(&committed));
    assert_image_blocks(
        &committed,
        &fixture.targets,
        fixture.block_size,
        &fixture.replacements,
    );
}

#[test]
fn recovery_ext4_clear_write_failure_keeps_ext4_recovery_feature() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture =
        journaled_update_image_bytes(&mke2fs, &debugfs, "recover-ext4-write-fail", 3, true);
    let replay_writes = fixture.targets.len() * fixture.device_writes_per_filesystem_block;
    let journal_empty_writes = fixture.device_writes_per_filesystem_block;
    let device = Arc::new(FaultyBufferedImageDevice::fail_write_at(
        fixture.bytes.clone(),
        replay_writes + journal_empty_writes + 1,
    ));

    let error = Ext4Filesystem::recover(device.clone()).expect_err("ext4 recovery clear fails");

    assert!(matches!(error, Ext4Error::Device(DriverError::Io)));
    let committed = device.committed_bytes();
    assert!(image_needs_recovery(&committed));
    assert_image_blocks(
        &committed,
        &fixture.targets,
        fixture.block_size,
        &fixture.replacements,
    );
}

#[test]
fn recovery_ext4_clear_flush_failure_keeps_ext4_recovery_feature() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let fixture =
        journaled_update_image_bytes(&mke2fs, &debugfs, "recover-ext4-flush-fail", 3, true);
    let device = Arc::new(FaultyBufferedImageDevice::fail_flush_at(
        fixture.bytes.clone(),
        3,
    ));

    let error = Ext4Filesystem::recover(device.clone()).expect_err("ext4 recovery flush fails");

    assert!(matches!(error, Ext4Error::Device(DriverError::Io)));
    let committed = device.committed_bytes();
    assert!(image_needs_recovery(&committed));
    assert_image_blocks(
        &committed,
        &fixture.targets,
        fixture.block_size,
        &fixture.replacements,
    );
}

#[test]
fn nonzero_journal_start_without_ext4_recovery_flag_still_mounts() {
    const JOURNAL_SUPERBLOCK_SIZE: usize = 1024;
    const FIRST_LOG_BLOCK_OFFSET: usize = 0x14;
    const START_OFFSET: usize = 0x1c;
    const INCOMPAT_OFFSET: usize = 0x28;
    const CHECKSUM_OFFSET: usize = 0xfc;
    const CSUM_V2_OR_V3: u32 = 0x0000_0018;

    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("journal-start");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.clone())))
        .expect("mount generated Linux ext4 image");
    let journal_block = filesystem
        .journal_superblock_block()
        .expect("map journal superblock");
    let journal_block = journal_block.expect("internal journal");
    let block_size = filesystem.layout().block_size() as usize;
    let offset = usize::try_from(journal_block.get())
        .expect("journal block fits usize")
        .checked_mul(block_size)
        .expect("journal byte offset");
    let journal = &mut bytes[offset..offset + JOURNAL_SUPERBLOCK_SIZE];
    let first_log_block = u32::from_be_bytes(
        journal[FIRST_LOG_BLOCK_OFFSET..FIRST_LOG_BLOCK_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    journal[START_OFFSET..START_OFFSET + 4].copy_from_slice(&first_log_block.to_be_bytes());

    let incompat = u32::from_be_bytes(
        journal[INCOMPAT_OFFSET..INCOMPAT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    if incompat & CSUM_V2_OR_V3 != 0 {
        journal[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].fill(0);
        let checksum = crc32c(u32::MAX, journal);
        journal[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_be_bytes());
    }

    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("s_start alone must not select ext4 recovery");
    assert_eq!(
        filesystem
            .journal_status()
            .expect("internal journal")
            .start_block(),
        Some(first_log_block)
    );
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn reads_linux_directory_and_regular_files() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("semantic");
    create_image(&mke2fs, &image);

    let hello = temporary_image_path("hello-host");
    let nested = temporary_image_path("nested-host");
    let sparse = temporary_image_path("sparse-host");
    fs::write(&hello, b"hello from linux ext4\n").expect("write host hello file");
    fs::write(&nested, b"nested directory payload").expect("write host nested file");
    let mut sparse_file = fs::File::create(&sparse).expect("create sparse host file");
    sparse_file
        .seek(SeekFrom::Start(8192))
        .expect("seek sparse host file");
    sparse_file.write_all(b"tail").expect("write sparse tail");
    run_debugfs(&debugfs, &image, "mkdir /subdir");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /hello.txt", hello.display()),
    );
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /subdir/nested.txt", nested.display()),
    );
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /sparse.bin", sparse.display()),
    );
    run_debugfs(&debugfs, &image, "punch /sparse.bin 0 1");
    run_debugfs(&debugfs, &image, "symlink /hello-link hello.txt");
    let long_target = "this/is/a/long/symlink/target/that/does/not/fit/in/i_block/storage";
    run_debugfs(
        &debugfs,
        &image,
        &format!("symlink /long-link {long_target}"),
    );

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.clone())))
        .expect("mount generated Linux ext4 image");
    let root = filesystem.root_inode().expect("read root inode");
    assert_eq!(root.kind(), InodeKind::Directory);

    let root_entries = filesystem.read_dir(&root).expect("read root directory");
    assert!(root_entries.iter().any(|entry| entry.name() == Some(".")));
    assert!(root_entries.iter().any(|entry| entry.name() == Some("..")));
    let hello_entry = filesystem
        .lookup(&root, "hello.txt")
        .expect("lookup hello")
        .expect("hello exists");
    assert_eq!(hello_entry.file_type(), DirectoryFileType::RegularFile);
    let hello_inode = filesystem
        .load_inode_private(hello_entry.inode())
        .expect("read hello inode");
    assert_eq!(hello_inode.kind(), InodeKind::RegularFile);
    let mut hello_bytes = vec![0; 64];
    let read = filesystem
        .read_at(&hello_inode, 0, &mut hello_bytes)
        .expect("read hello file");
    assert_eq!(&hello_bytes[..read], b"hello from linux ext4\n");
    assert_eq!(
        filesystem
            .read_at(&hello_inode, hello_inode.size(), &mut hello_bytes)
            .expect("read hello EOF"),
        0
    );

    let sparse_entry = filesystem
        .lookup(&root, "sparse.bin")
        .expect("lookup sparse")
        .expect("sparse exists");
    let sparse_inode = filesystem
        .load_inode_private(sparse_entry.inode())
        .expect("read sparse inode");
    let mut sparse_bytes = vec![0xff; 8196];
    let read = filesystem
        .read_at(&sparse_inode, 0, &mut sparse_bytes)
        .expect("read sparse file");
    assert_eq!(read, 8196);
    assert!(sparse_bytes[..8192].iter().all(|byte| *byte == 0));
    assert_eq!(&sparse_bytes[8192..8196], b"tail");

    let subdir_entry = filesystem
        .lookup(&root, "subdir")
        .expect("lookup subdir")
        .expect("subdir exists");
    assert_eq!(subdir_entry.file_type(), DirectoryFileType::Directory);
    let subdir = filesystem
        .load_inode_private(subdir_entry.inode())
        .expect("read subdir inode");
    let nested_entry = filesystem
        .lookup(&subdir, "nested.txt")
        .expect("lookup nested")
        .expect("nested exists");
    let nested_inode = filesystem
        .load_inode_private(nested_entry.inode())
        .expect("read nested inode");
    let mut nested_bytes = vec![0; 64];
    let read = filesystem
        .read_at(&nested_inode, 0, &mut nested_bytes)
        .expect("read nested file");
    assert_eq!(&nested_bytes[..read], b"nested directory payload");

    let symlink_entry = filesystem
        .lookup(&root, "hello-link")
        .expect("lookup symbolic link")
        .expect("symbolic link exists");
    let symlink_inode = filesystem
        .load_inode_private(symlink_entry.inode())
        .expect("read symbolic link inode");
    assert_eq!(symlink_inode.kind(), InodeKind::Symlink);
    let mut target = [0; 32];
    let read = filesystem
        .read_link_at(&symlink_inode, 0, &mut target)
        .expect("read symbolic link");
    assert_eq!(&target[..read], b"hello.txt");

    let long_symlink_entry = filesystem
        .lookup(&root, "long-link")
        .expect("lookup long symbolic link")
        .expect("long symbolic link exists");
    let long_symlink_inode = filesystem
        .load_inode_private(long_symlink_entry.inode())
        .expect("read long symbolic link inode");
    assert_eq!(long_symlink_inode.kind(), InodeKind::Symlink);
    let mut target = vec![0; long_symlink_inode.size() as usize];
    let read = filesystem
        .read_link_at(&long_symlink_inode, 0, &mut target)
        .expect("read long symbolic link");
    assert_eq!(read, long_target.len());
    assert_eq!(&target, long_target.as_bytes());

    fs::remove_file(hello).expect("remove host hello file");
    fs::remove_file(nested).expect("remove host nested file");
    fs::remove_file(sparse).expect("remove host sparse file");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn read_dir_from_resumes_with_ext4_directory_offsets() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("dir-resume");
    let payload = temporary_image_path("dir-resume-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"x").expect("write directory payload");
    for name in ["alpha", "beta", "gamma"] {
        run_debugfs(
            &debugfs,
            &image,
            &format!("write {} /{name}", payload.display()),
        );
    }

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount generated Linux ext4 image");
    let root = filesystem.root_inode().expect("read root inode");
    let expected: Vec<Vec<u8>> = filesystem
        .read_dir(&root)
        .expect("read root directory")
        .into_iter()
        .map(|entry| Vec::from(entry.name_bytes()))
        .collect();

    let mut position = Ext4DirPos::new(0);
    let mut resumed = Vec::new();
    loop {
        let mut sink = LimitedDirSink::new(1);
        let next = filesystem
            .read_dir_from(&root, position, &mut sink)
            .expect("resume directory read");
        if sink.entries.is_empty() {
            break;
        }
        assert!(next.get() > position.get());
        resumed.extend(sink.entries.into_iter().map(|entry| entry.name));
        position = next;
    }
    assert_eq!(resumed, expected);

    let mut first = LimitedDirSink::new(1);
    filesystem
        .read_dir_from(&root, Ext4DirPos::new(0), &mut first)
        .expect("read first directory entry");
    let mut non_boundary = LimitedDirSink::new(1);
    let error = filesystem
        .read_dir_from(&root, Ext4DirPos::new(1), &mut non_boundary)
        .expect_err("reject non-boundary directory offset");
    assert_eq!(error, Ext4Error::InvalidDirectoryPosition);
    assert!(non_boundary.entries.is_empty());

    fs::remove_file(payload).expect("remove directory payload");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn reads_linux_inode_and_external_xattrs() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("xattr");
    let payload = temporary_image_path("xattr-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"xattr target").expect("write xattr payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /file", payload.display()),
    );
    run_debugfs(&debugfs, &image, "ea_set /file user.alpha bravo");
    let large_value = "z".repeat(900);
    run_debugfs(
        &debugfs,
        &image,
        &format!("ea_set /file user.large {large_value}"),
    );

    let bytes = fs::read(&image).expect("read generated xattr image");
    let mut corrupt_bytes = bytes.clone();
    let large_offset = corrupt_bytes
        .windows(large_value.len())
        .position(|window| window == large_value.as_bytes())
        .expect("large xattr value is stored in the image");
    corrupt_bytes[large_offset] ^= 1;
    let corrupt = Ext4Filesystem::mount(Arc::new(ImageDevice::new(corrupt_bytes)))
        .expect("mount xattr image with corrupted external xattr block");
    let corrupt_root = corrupt.root_inode().expect("read corrupt xattr root inode");
    let corrupt_entry = corrupt
        .lookup(&corrupt_root, "file")
        .expect("lookup corrupt xattr file")
        .expect("corrupt xattr file exists");
    let corrupt_inode = corrupt
        .load_inode_private(corrupt_entry.inode())
        .expect("read corrupt xattr inode");
    assert!(matches!(
        corrupt.read_xattrs(&corrupt_inode),
        Err(Ext4Error::ChecksumMismatch {
            target: kext4::ChecksumTarget::XattrBlock { .. },
            ..
        })
    ));

    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount generated xattr ext4 image");
    let root = filesystem.root_inode().expect("read root inode");
    let entry = filesystem
        .lookup(&root, "file")
        .expect("lookup xattr file")
        .expect("xattr file exists");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read xattr inode");

    let alpha = filesystem
        .get_xattr(&inode, Ext4XattrNamespace::User, b"alpha")
        .expect("read inline xattr")
        .expect("inline xattr exists");
    assert_eq!(alpha.as_slice(), b"bravo");
    let large = filesystem
        .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
        .expect("read external xattr")
        .expect("external xattr exists");
    assert_eq!(large.as_slice(), large_value.as_bytes());
    assert!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"missing")
            .expect("read missing xattr")
            .is_none()
    );

    let xattrs = filesystem.read_xattrs(&inode).expect("list xattrs");
    assert!(xattrs.iter().any(|xattr| {
        xattr.namespace() == Ext4XattrNamespace::User
            && xattr.name_bytes() == b"alpha"
            && xattr.value() == b"bravo"
    }));
    assert!(xattrs.iter().any(|xattr| {
        xattr.namespace() == Ext4XattrNamespace::User
            && xattr.name_bytes() == b"large"
            && xattr.value() == large_value.as_bytes()
    }));

    fs::remove_file(payload).expect("remove xattr payload");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn journals_inline_xattr_set_and_remove() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("xattr-write");
    let payload = temporary_image_path("xattr-write-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"xattr write target").expect("write xattr payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /file", payload.display()),
    );

    let device = Arc::new(WritableImageDevice::new(
        fs::read(&image).expect("read generated xattr write image"),
    ));
    let timestamp = Ext4Timestamp::new(1_720_000_000, 0);
    let inode_number = {
        let mut filesystem =
            Ext4Filesystem::mount(device.clone()).expect("mount xattr write image");
        let root = filesystem.root_inode().expect("read root inode");
        let entry = filesystem
            .lookup(&root, "file")
            .expect("lookup xattr write file")
            .expect("xattr write file exists");
        let inode = filesystem
            .load_inode_private(entry.inode())
            .expect("read xattr write inode");
        filesystem
            .set_xattr(
                &inode,
                Ext4XattrNamespace::User,
                b"codex",
                b"inline-value",
                timestamp,
            )
            .expect("set inline xattr");
        assert_eq!(
            filesystem
                .get_xattr(&inode, Ext4XattrNamespace::User, b"codex")
                .expect("read updated xattr")
                .as_deref(),
            Some(&b"inline-value"[..])
        );
        let committed_after_set = device.committed_bytes();
        let ctime_after_set = inode.ctime();
        filesystem
            .set_xattr(
                &inode,
                Ext4XattrNamespace::User,
                b"codex",
                b"inline-value",
                Ext4Timestamp::new(1_720_000_001, 0),
            )
            .expect("setting the same xattr value is a no-op");
        assert_eq!(inode.ctime(), ctime_after_set);
        assert_eq!(device.committed_bytes(), committed_after_set);
        filesystem
            .sync_filesystem()
            .expect("persist inline xattr set");
        inode.number()
    };

    {
        let filesystem = Ext4Filesystem::mount(device.clone()).expect("remount after xattr set");
        let persisted = filesystem
            .load_inode_private(inode_number)
            .expect("read persisted xattr inode");
        assert_eq!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::User, b"codex")
                .expect("read persisted xattr")
                .as_deref(),
            Some(&b"inline-value"[..])
        );
    }

    {
        let mut filesystem = Ext4Filesystem::mount(device.clone()).expect("mount for xattr remove");
        let persisted = filesystem
            .load_inode_private(inode_number)
            .expect("read xattr inode before remove");
        filesystem
            .remove_xattr(
                &persisted,
                Ext4XattrNamespace::User,
                b"codex",
                Ext4Timestamp::new(1_720_000_002, 0),
            )
            .expect("remove inline xattr");
        assert!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::User, b"codex")
                .expect("read removed xattr")
                .is_none()
        );
        filesystem
            .sync_filesystem()
            .expect("persist inline xattr removal");
    }

    let filesystem = Ext4Filesystem::mount(device).expect("remount after xattr remove");
    let root = filesystem
        .root_inode()
        .expect("read root inode after remove");
    let entry = filesystem
        .lookup(&root, "file")
        .expect("lookup xattr file after remove")
        .expect("xattr file exists after remove");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read xattr inode after remove");
    assert!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"codex")
            .expect("read missing xattr after remove")
            .is_none()
    );

    fs::remove_file(payload).expect("remove xattr write payload");
    fs::remove_file(image).expect("remove generated xattr write image");
}

#[test]
fn journals_external_xattr_block_and_acl_round_trips() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let e2fsck = require_e2fsprogs("e2fsck");
    let image = temporary_image_path("xattr-external-write");
    let payload = temporary_image_path("xattr-external-write-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"external xattr write target").expect("write xattr payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /file", payload.display()),
    );

    let device = Arc::new(WritableImageDevice::new(
        fs::read(&image).expect("read generated external xattr image"),
    ));
    let large_value: Vec<u8> = (0u8..=250).cycle().take(603).collect();
    let acl_value = vec![0x02, 0x00, 0x00, 0x00];
    let (inode_number, original_blocks, external_blocks) = {
        let mut filesystem =
            Ext4Filesystem::mount(device.clone()).expect("mount external xattr write image");
        let root = filesystem.root_inode().expect("read root inode");
        let entry = filesystem
            .lookup(&root, "file")
            .expect("lookup external xattr write file")
            .expect("external xattr write file exists");
        let inode = filesystem
            .load_inode_private(entry.inode())
            .expect("read external xattr write inode");
        let original_blocks = inode.blocks();
        let external_blocks = u64::from(filesystem.layout().block_size()) / 512;
        filesystem
            .set_xattr(
                &inode,
                Ext4XattrNamespace::User,
                b"large",
                &large_value,
                Ext4Timestamp::new(1_720_000_010, 0),
            )
            .expect("set large xattr into external block");
        assert_eq!(inode.blocks(), original_blocks + external_blocks);
        assert_eq!(
            filesystem
                .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
                .expect("read external xattr")
                .as_deref(),
            Some(large_value.as_slice())
        );
        filesystem
            .set_xattr(
                &inode,
                Ext4XattrNamespace::PosixAclAccess,
                b"",
                &acl_value,
                Ext4Timestamp::new(1_720_000_011, 0),
            )
            .expect("set opaque ACL xattr");
        assert_eq!(
            filesystem
                .get_xattr(&inode, Ext4XattrNamespace::PosixAclAccess, b"")
                .expect("read opaque ACL xattr")
                .as_deref(),
            Some(acl_value.as_slice())
        );
        filesystem
            .sync_filesystem()
            .expect("persist external xattr updates");
        (inode.number(), original_blocks, external_blocks)
    };

    fs::write(&image, device.committed_bytes()).expect("persist external xattr hash image");
    run_e2fsck_check(&e2fsck, &image);

    {
        let filesystem = Ext4Filesystem::mount(device.clone()).expect("remount external xattr");
        let persisted = filesystem
            .load_inode_private(inode_number)
            .expect("read persisted external xattr inode");
        assert_eq!(persisted.blocks(), original_blocks + external_blocks);
        assert_eq!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::User, b"large")
                .expect("read persisted external xattr")
                .as_deref(),
            Some(large_value.as_slice())
        );
        assert_eq!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::PosixAclAccess, b"")
                .expect("read persisted ACL xattr")
                .as_deref(),
            Some(acl_value.as_slice())
        );
    }

    {
        let mut filesystem =
            Ext4Filesystem::mount(device.clone()).expect("mount for external xattr remove");
        let persisted = filesystem
            .load_inode_private(inode_number)
            .expect("read external xattr inode before remove");
        filesystem
            .remove_xattr(
                &persisted,
                Ext4XattrNamespace::User,
                b"large",
                Ext4Timestamp::new(1_720_000_012, 0),
            )
            .expect("remove large xattr");
        assert_eq!(persisted.blocks(), original_blocks);
        assert_eq!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::PosixAclAccess, b"")
                .expect("read remaining ACL xattr")
                .as_deref(),
            Some(acl_value.as_slice())
        );
        filesystem
            .remove_xattr(
                &persisted,
                Ext4XattrNamespace::PosixAclAccess,
                b"",
                Ext4Timestamp::new(1_720_000_013, 0),
            )
            .expect("remove ACL xattr and release external block");
        assert_eq!(persisted.blocks(), original_blocks);
        assert!(
            filesystem
                .get_xattr(&persisted, Ext4XattrNamespace::User, b"large")
                .expect("read missing large xattr")
                .is_none()
        );
        filesystem
            .sync_filesystem()
            .expect("persist external xattr removals");
    }

    let filesystem = Ext4Filesystem::mount(device).expect("remount after external xattr remove");
    let inode = filesystem
        .load_inode_private(inode_number)
        .expect("read inode after external xattr remove");
    assert_eq!(inode.blocks(), original_blocks);

    fs::remove_file(payload).expect("remove external xattr payload");
    fs::remove_file(image).expect("remove generated external xattr image");
}

#[test]
fn unlinks_and_rename_overwrites_external_xattr_inodes() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let e2fsck = require_e2fsprogs("e2fsck");
    let image = temporary_image_path("xattr-external-namei");
    let payload = temporary_image_path("xattr-external-namei-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"external xattr namei target").expect("write xattr namei payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /unlink-me", payload.display()),
    );
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /overwrite-me", payload.display()),
    );
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /replacement", payload.display()),
    );

    let device = Arc::new(WritableImageDevice::new(
        fs::read(&image).expect("read generated external xattr namei image"),
    ));
    let large_value = vec![b'n'; 600];
    {
        let mut filesystem =
            Ext4Filesystem::mount(device.clone()).expect("mount external xattr namei image");
        let root = filesystem.root_inode().expect("read root inode");
        let unlink_entry = filesystem
            .lookup(&root, "unlink-me")
            .expect("lookup external xattr unlink target")
            .expect("external xattr unlink target exists");
        let unlink_inode = filesystem
            .load_inode_private(unlink_entry.inode())
            .expect("read external xattr unlink target inode");
        filesystem
            .set_xattr(
                &unlink_inode,
                Ext4XattrNamespace::User,
                b"large",
                &large_value,
                Ext4Timestamp::new(1_720_000_030, 0),
            )
            .expect("set external xattr before unlink");
        assert_eq!(
            filesystem
                .get_xattr(&unlink_inode, Ext4XattrNamespace::User, b"large")
                .expect("read external xattr before unlink")
                .as_deref(),
            Some(large_value.as_slice())
        );

        filesystem
            .unlink(
                &root,
                b"unlink-me",
                &unlink_inode,
                Ext4Timestamp::new(1_720_000_031, 0),
            )
            .expect("unlink inode with external xattr block");
        assert!(
            filesystem
                .lookup(&root, "unlink-me")
                .expect("lookup removed external xattr file")
                .is_none()
        );
        filesystem
            .evict_unlinked_inode(&unlink_inode, Ext4Timestamp::new(1_720_000_031, 1))
            .expect("evict unlinked inode with external xattr block");

        let overwrite_entry = filesystem
            .lookup(&root, "overwrite-me")
            .expect("lookup external xattr rename target")
            .expect("external xattr rename target exists");
        let overwrite_inode = filesystem
            .load_inode_private(overwrite_entry.inode())
            .expect("read external xattr rename target inode");
        filesystem
            .set_xattr(
                &overwrite_inode,
                Ext4XattrNamespace::User,
                b"large",
                &large_value,
                Ext4Timestamp::new(1_720_000_032, 0),
            )
            .expect("set external xattr before rename overwrite");
        assert_eq!(
            filesystem
                .get_xattr(&overwrite_inode, Ext4XattrNamespace::User, b"large")
                .expect("read external xattr before rename overwrite")
                .as_deref(),
            Some(large_value.as_slice())
        );

        let replacement_entry = filesystem
            .lookup(&root, "replacement")
            .expect("lookup rename source")
            .expect("rename source exists");
        let replacement_inode = filesystem
            .load_inode_private(replacement_entry.inode())
            .expect("read rename source inode");
        filesystem
            .rename(
                &root,
                b"replacement",
                &replacement_inode,
                &root,
                b"overwrite-me",
                Some(&overwrite_inode),
                Ext4Timestamp::new(1_720_000_033, 0),
            )
            .expect("rename over inode with external xattr block");
        assert!(
            filesystem
                .lookup(&root, "replacement")
                .expect("lookup moved-away replacement name")
                .is_none()
        );
        let moved_entry = filesystem
            .lookup(&root, "overwrite-me")
            .expect("lookup rename replacement")
            .expect("rename replacement exists");
        let moved_inode = filesystem
            .load_inode_private(moved_entry.inode())
            .expect("read rename replacement inode");
        assert!(
            filesystem
                .get_xattr(&moved_inode, Ext4XattrNamespace::User, b"large")
                .expect("read replacement xattr")
                .is_none()
        );
        filesystem
            .evict_unlinked_inode(&overwrite_inode, Ext4Timestamp::new(1_720_000_033, 1))
            .expect("evict overwritten inode with external xattr block");
        filesystem
            .sync_filesystem()
            .expect("persist external xattr namespace mutations");
    }

    fs::write(&image, device.committed_bytes()).expect("persist mutated external xattr image");
    run_e2fsck_check(&e2fsck, &image);
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(
        fs::read(&image).expect("read remounted external xattr namei image"),
    )))
    .expect("remount external xattr namei image");
    let root = filesystem.root_inode().expect("read remounted root");
    assert!(
        filesystem
            .lookup(&root, "unlink-me")
            .expect("lookup unlinked external xattr inode after remount")
            .is_none()
    );
    assert!(
        filesystem
            .lookup(&root, "replacement")
            .expect("lookup old replacement after remount")
            .is_none()
    );
    assert!(
        filesystem
            .lookup(&root, "overwrite-me")
            .expect("lookup overwritten name after remount")
            .is_some()
    );

    fs::remove_file(payload).expect("remove external xattr namei payload");
    fs::remove_file(image).expect("remove generated external xattr namei image");
}

#[test]
fn failed_external_xattr_mutation_does_not_publish_half_update_and_retries() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("xattr-external-fault");
    let payload = temporary_image_path("xattr-external-fault-payload");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"external xattr fault target").expect("write xattr fault payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /file", payload.display()),
    );

    let original = fs::read(&image).expect("read generated external xattr fault image");
    let large_value = vec![b'f'; 600];
    for fault in [
        XattrFault::Write {
            device: Arc::new(FaultyBufferedImageDevice::fail_write_at(
                original.clone(),
                1,
            )),
        },
        XattrFault::Flush {
            device: Arc::new(FaultyBufferedImageDevice::fail_flush_at(
                original.clone(),
                1,
            )),
        },
    ] {
        let error = {
            let mut filesystem =
                Ext4Filesystem::mount(fault.device()).expect("mount faulty xattr image");
            set_large_xattr_on_file(&mut filesystem, &large_value)
                .expect_err("faulted xattr mutation must fail")
        };
        assert!(matches!(
            error,
            Ext4Error::Device(DriverError::Io) | Ext4Error::JournalAborted
        ));

        let committed = fault.committed_bytes();
        assert_large_xattr_absent(&committed);
        let retry_device = Arc::new(WritableImageDevice::new(committed));
        let mut filesystem =
            Ext4Filesystem::mount(retry_device).expect("remount retryable xattr image");
        let updated = set_large_xattr_on_file(&mut filesystem, &large_value)
            .expect("retry external xattr mutation");
        assert_eq!(
            filesystem
                .get_xattr(&updated, Ext4XattrNamespace::User, b"large")
                .expect("read retried external xattr")
                .as_deref(),
            Some(large_value.as_slice())
        );
    }

    fs::remove_file(payload).expect("remove external xattr fault payload");
    fs::remove_file(image).expect("remove external xattr fault image");
}

#[test]
fn reads_linux_legacy_indirect_file_blocks() {
    const BLOCK_SIZE: usize = 1024;
    const POINTERS_PER_BLOCK: u64 = (BLOCK_SIZE / 4) as u64;
    const DIRECT_LAST: u64 = 11;
    const SINGLE_FIRST: u64 = 12;
    const SINGLE_LAST: u64 = SINGLE_FIRST + POINTERS_PER_BLOCK - 1;
    const DOUBLE_FIRST: u64 = SINGLE_LAST + 1;
    const DOUBLE_CROSS: u64 = DOUBLE_FIRST + POINTERS_PER_BLOCK;
    const DOUBLE_LAST: u64 = DOUBLE_FIRST + POINTERS_PER_BLOCK * POINTERS_PER_BLOCK - 1;
    const TRIPLE_FIRST: u64 = DOUBLE_LAST + 1;
    const BLOCK_COUNT: u64 = TRIPLE_FIRST + 1;

    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("legacy-indirect");
    let payload = temporary_image_path("legacy-indirect-payload");
    create_image_with_layout_features(
        &mke2fs,
        &image,
        256 * 1024 * 1024,
        BLOCK_SIZE as u32,
        true,
        None,
        false,
        false,
    );

    let payload_file = fs::File::create(&payload).expect("create legacy indirect payload");
    let mut payload_writer = BufWriter::new(payload_file);
    let mut block = vec![0; BLOCK_SIZE];
    for logical in 0..BLOCK_COUNT {
        block.fill(legacy_payload_byte(logical));
        payload_writer
            .write_all(&block)
            .expect("write legacy indirect payload block");
    }
    payload_writer
        .flush()
        .expect("flush legacy indirect payload");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /legacy.bin", payload.display()),
    );

    let bytes = fs::read(&image).expect("read generated legacy indirect image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.clone())))
        .expect("mount generated legacy indirect ext4 image");
    let root = filesystem.root_inode().expect("read root inode");
    let entry = filesystem
        .lookup(&root, "legacy.bin")
        .expect("lookup legacy indirect file")
        .expect("legacy indirect file exists");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read legacy indirect inode");
    assert_eq!(inode.flags() & 0x0008_0000, 0);

    for logical in [
        0,
        DIRECT_LAST,
        SINGLE_FIRST,
        SINGLE_LAST,
        DOUBLE_FIRST,
        DOUBLE_CROSS,
        DOUBLE_LAST,
        TRIPLE_FIRST,
    ] {
        assert_legacy_logical_block(&filesystem, &inode, logical, BLOCK_SIZE);
    }
    assert!(matches!(
        filesystem.map_blocks(&inode, LogicalBlock::new(TRIPLE_FIRST + 1)),
        Ok(BlockMapping::Hole { .. })
    ));

    let single_indirect = legacy_inode_pointer(&inode, 12);
    let single_first_offset = image_block_entry_offset(u64::from(single_indirect), 0, BLOCK_SIZE);

    let mut system_zone_pointer = bytes.clone();
    put_le_u32(&mut system_zone_pointer, single_first_offset, 1);
    let (corrupt, corrupt_inode) =
        legacy_indirect_inode_from_image(system_zone_pointer, "legacy.bin");
    assert_eq!(
        corrupt.map_blocks(&corrupt_inode, LogicalBlock::new(SINGLE_FIRST)),
        Err(Ext4Error::Corrupt(kext4::CorruptKind::InvalidExtent))
    );

    let mut out_of_bounds_pointer = bytes;
    let invalid_block =
        u32::try_from(filesystem.layout().block_count() + 16).expect("block count fits u32");
    put_le_u32(
        &mut out_of_bounds_pointer,
        single_first_offset,
        invalid_block,
    );
    let (corrupt, corrupt_inode) =
        legacy_indirect_inode_from_image(out_of_bounds_pointer, "legacy.bin");
    assert_eq!(
        corrupt.map_blocks(&corrupt_inode, LogicalBlock::new(SINGLE_FIRST)),
        Err(Ext4Error::Corrupt(kext4::CorruptKind::InvalidExtent))
    );

    fs::remove_file(payload).expect("remove legacy indirect payload");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn statfs_matches_dumpe2fs_for_one_kib_and_four_kib_images() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let dumpe2fs = require_e2fsprogs("dumpe2fs");

    for block_size in [1024, 4096] {
        let image = temporary_image_path(&format!("statfs-{block_size}"));
        create_image_with_layout(
            &mke2fs,
            &image,
            64 * 1024 * 1024,
            block_size,
            true,
            Some(10),
        );

        let expected = read_dumpe2fs_stats(&dumpe2fs, &image);
        let bytes = fs::read(&image).expect("read generated ext4 image");
        let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
            .expect("mount generated Linux ext4 image");
        let stat = filesystem.statfs().expect("read kext4 statfs");
        let minix_stat = filesystem
            .statfs_with_mode(Ext4StatFsMode::Minix)
            .expect("read kext4 minixdf statfs");

        assert_eq!(stat.block_size, expected.block_size as u32);
        assert_eq!(stat.fragment_size, expected.fragment_size as u32);
        assert_eq!(
            stat.blocks,
            expected
                .block_count
                .checked_sub(expected.overhead_clusters)
                .expect("dumpe2fs overhead does not exceed block count")
        );
        assert_eq!(stat.blocks_free, expected.free_blocks);
        assert_eq!(
            stat.blocks_available,
            expected
                .free_blocks
                .saturating_sub(expected.reserved_block_count)
        );
        assert_eq!(stat.files, expected.inode_count);
        assert_eq!(stat.files_free, expected.free_inodes);
        assert_eq!(stat.max_name_len, 255);
        assert_eq!(minix_stat.blocks, expected.block_count);
        assert_eq!(minix_stat.blocks_free, stat.blocks_free);
        assert_eq!(minix_stat.blocks_available, stat.blocks_available);
        assert_eq!(minix_stat.files, stat.files);
        assert_eq!(minix_stat.files_free, stat.files_free);

        fs::remove_file(image).expect("remove generated image");
    }
}

#[test]
fn rejects_public_access_to_reserved_journal_inode() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("reserved-inode");
    create_image(&mke2fs, &image);

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount generated Linux ext4 image");
    assert!(matches!(
        filesystem.load_inode_private(InodeNumber::new(8)),
        Err(Ext4Error::Unsupported(UnsupportedKind::ReservedInode))
    ));
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn reads_linux_htree_directory() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let e2fsck = require_e2fsprogs("e2fsck");
    let image = temporary_image_path("htree");
    let payload = temporary_image_path("htree-payload");
    let script = temporary_image_path("htree-debugfs");
    create_image(&mke2fs, &image);
    fs::write(&payload, b"x").expect("write htree payload");

    let mut commands = String::from("mkdir /big\n");
    for index in 0..1_200 {
        commands.push_str(&format!("write {} /big/f{index:04}\n", payload.display()));
    }
    fs::write(&script, commands).expect("write debugfs command file");
    run_debugfs_script(&debugfs, &image, &script);
    run_e2fsck_optimize_directories(&e2fsck, &image);

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes.clone())))
        .expect("mount generated Linux ext4 image");
    let root = filesystem.root_inode().expect("read root inode");
    let big_entry = filesystem
        .lookup(&root, "big")
        .expect("lookup big")
        .expect("big exists");
    let big = filesystem
        .load_inode_private(big_entry.inode())
        .expect("read big inode");
    assert_ne!(big.flags() & 0x0000_1000, 0);

    let entries = filesystem.read_dir(&big).expect("read htree directory");
    let names: Vec<Vec<u8>> = entries
        .iter()
        .map(|entry| Vec::from(entry.name_bytes()))
        .collect();
    assert!(names.contains(&b".".to_vec()));
    assert!(names.contains(&b"..".to_vec()));
    assert!(names.contains(&b"f0000".to_vec()));
    assert!(names.contains(&b"f1199".to_vec()));
    assert_eq!(names.len(), 1_202);

    let first = filesystem
        .lookup(&big, "f0000")
        .expect("lookup first htree leaf entry")
        .expect("first htree leaf entry exists");
    assert_eq!(first.file_type(), DirectoryFileType::RegularFile);
    let last = filesystem
        .lookup(&big, "f1199")
        .expect("lookup last htree leaf entry")
        .expect("last htree leaf entry exists");
    assert_eq!(last.file_type(), DirectoryFileType::RegularFile);
    assert!(
        filesystem
            .lookup(&big, "missing")
            .expect("lookup missing htree entry")
            .is_none()
    );

    let root_block = match filesystem
        .map_blocks(&big, LogicalBlock::new(0))
        .expect("map htree root block")
    {
        BlockMapping::Mapped { physical, .. } => physical.get(),
        mapping => panic!("htree root block must be mapped, got {mapping:?}"),
    };
    let block_size = filesystem.layout().block_size() as usize;
    let checksum_offset = usize::try_from(root_block)
        .expect("htree root block fits usize")
        .checked_mul(block_size)
        .and_then(|offset| offset.checked_add(block_size - 4))
        .expect("htree root checksum offset");
    let mut corrupt = bytes.clone();
    corrupt[checksum_offset] ^= 1;
    let corrupt = Ext4Filesystem::mount(Arc::new(ImageDevice::new(corrupt)))
        .expect("mount htree root checksum image");
    let corrupt_root = corrupt.root_inode().expect("read corrupt htree root inode");
    let corrupt_big_entry = corrupt
        .lookup(&corrupt_root, "big")
        .expect("lookup corrupt big")
        .expect("corrupt big exists");
    let corrupt_big = corrupt
        .load_inode_private(corrupt_big_entry.inode())
        .expect("read corrupt big inode");
    assert!(matches!(
        corrupt.read_dir(&corrupt_big),
        Err(Ext4Error::ChecksumMismatch {
            target: kext4::ChecksumTarget::DirectoryBlock {
                inode,
                block: 0
            },
            ..
        }) if inode == big.number().get()
    ));

    let mut position = Ext4DirPos::new(0);
    let mut resumed = Vec::new();
    loop {
        let mut sink = LimitedDirSink::new(1);
        let next = filesystem
            .read_dir_from(&big, position, &mut sink)
            .expect("resume htree directory read");
        if sink.entries.is_empty() {
            break;
        }
        assert!(next.get() > position.get());
        resumed.extend(sink.entries.into_iter().map(|entry| entry.name));
        position = next;
    }
    assert_eq!(resumed, names);

    fs::remove_file(payload).expect("remove htree payload");
    fs::remove_file(script).expect("remove debugfs command file");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_corrupt_superblock_checksum() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("bad-super");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes[1024 + 0x10] ^= 1;
    let error = match Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("corrupt superblock unexpectedly mounted"),
        Err(error) => error,
    };
    assert!(matches!(error, Ext4Error::ChecksumMismatch { .. }));
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_corrupt_group_descriptor_checksum() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("bad-group");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes[4096 + 12] ^= 1;
    let error = match Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("corrupt group descriptor unexpectedly mounted"),
        Err(error) => error,
    };
    assert!(matches!(error, Ext4Error::ChecksumMismatch { .. }));
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_device_shorter_than_superblock_geometry() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let image = temporary_image_path("short-device");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes.truncate(bytes.len() / 2);
    let error = match Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("truncated block device unexpectedly mounted"),
        Err(error) => error,
    };
    assert_eq!(error, Ext4Error::OutOfBounds);
    fs::remove_file(image).expect("remove generated image");
}

fn create_image(mke2fs: &Path, image: &Path) {
    create_image_with_journal(mke2fs, image, true);
}

fn create_image_with_journal(mke2fs: &Path, image: &Path, has_journal: bool) {
    create_image_with_layout(mke2fs, image, 256 * 1024 * 1024, 4096, has_journal, None);
}

fn create_image_with_layout(
    mke2fs: &Path,
    image: &Path,
    size: u64,
    block_size: u32,
    has_journal: bool,
    reserved_percent: Option<u8>,
) {
    create_image_with_layout_and_64bit(
        mke2fs,
        image,
        size,
        block_size,
        has_journal,
        reserved_percent,
        true,
    );
}

fn create_image_with_layout_and_64bit(
    mke2fs: &Path,
    image: &Path,
    size: u64,
    block_size: u32,
    has_journal: bool,
    reserved_percent: Option<u8>,
    has_64bit: bool,
) {
    create_image_with_layout_features(
        mke2fs,
        image,
        size,
        block_size,
        has_journal,
        reserved_percent,
        has_64bit,
        true,
    );
}

fn create_image_with_layout_features(
    mke2fs: &Path,
    image: &Path,
    size: u64,
    block_size: u32,
    has_journal: bool,
    reserved_percent: Option<u8>,
    has_64bit: bool,
    has_extents: bool,
) {
    let file = fs::File::create(image).expect("create ext4 image");
    file.set_len(size).expect("size ext4 image");
    let journal_feature = if has_journal {
        "has_journal"
    } else {
        "^has_journal"
    };
    let block_number_feature = if has_64bit { "64bit" } else { "^64bit" };
    let extent_feature = if has_extents { "extent" } else { "^extent" };
    let features = format!(
        "{journal_feature},{extent_feature},filetype,{block_number_feature},flex_bg,metadata_csum,\
         dir_index,^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
         ^verity,^casefold,^mmp"
    );
    let mut command = Command::new(mke2fs);
    command
        .args(["-q", "-t", "ext4", "-F", "-b"])
        .arg(block_size.to_string())
        .args(["-I", "256"]);
    if let Some(reserved_percent) = reserved_percent {
        command.arg("-m").arg(reserved_percent.to_string());
    }
    let status = command
        .arg("-O")
        .arg(&features)
        .arg(image)
        .status()
        .expect("run mke2fs");
    assert!(status.success(), "mke2fs failed");
}

fn journaled_update_image_bytes(
    mke2fs: &Path,
    debugfs: &Path,
    label: &str,
    journal_version: u8,
    has_64bit: bool,
) -> JournaledUpdateImage {
    const BLOCK_SIZE: usize = 4096;
    const UPDATE_COUNT: usize = 2;

    let image = temporary_image_path(label);
    let host = temporary_image_path(&format!("{label}-host"));
    let script = temporary_image_path(&format!("{label}-script"));
    let payloads = (0..UPDATE_COUNT)
        .map(|index| temporary_image_path(&format!("{label}-payload-{index}")))
        .collect::<Vec<_>>();

    create_image_with_layout_and_64bit(
        mke2fs,
        &image,
        256 * 1024 * 1024,
        BLOCK_SIZE as u32,
        true,
        None,
        has_64bit,
    );
    fs::write(&host, vec![b'a'; BLOCK_SIZE * UPDATE_COUNT]).expect("write host journal file");
    run_debugfs(
        debugfs,
        &image,
        &format!("write {} /journaled.bin", host.display()),
    );

    let before = fs::read(&image).expect("read pre-journal image");
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(before.clone())))
        .expect("mount pre-journal image");
    let root = filesystem.root_inode().expect("read root inode");
    let entry = filesystem
        .lookup(&root, "journaled.bin")
        .expect("lookup journaled file")
        .expect("journaled file exists");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read journaled file inode");

    let mut targets = Vec::new();
    let mut originals = Vec::new();
    let mut replacements = Vec::new();
    for logical in 0..UPDATE_COUNT {
        let target = match filesystem
            .map_blocks(&inode, LogicalBlock::new(logical as u64))
            .expect("map journaled file block")
        {
            BlockMapping::Mapped { physical, .. } => physical.get(),
            mapping => panic!("journaled file block must be mapped, got {mapping:?}"),
        };
        let original = image_block(&before, target, BLOCK_SIZE);
        let mut replacement = original.clone();
        let marker = format!("kext4 journal v{journal_version} 64bit={has_64bit} block={logical}");
        replacement[..marker.len()].copy_from_slice(marker.as_bytes());
        fs::write(&payloads[logical], &replacement).expect("write journal payload");

        targets.push(target);
        originals.push(original);
        replacements.push(replacement);
    }

    let mut script_text = format!("journal_open -c -v {journal_version}\n");
    for (target, payload) in targets.iter().zip(&payloads) {
        script_text.push_str(&format!(
            "journal_write -b {target} {}\n",
            payload.display()
        ));
    }
    script_text.push_str("journal_close\n");
    fs::write(&script, script_text).expect("write debugfs journal script");
    run_debugfs_script(debugfs, &image, &script);

    let bytes = fs::read(&image).expect("read image with journal transaction");
    assert!(image_needs_recovery(&bytes));
    for (target, original) in targets.iter().zip(&originals) {
        assert_eq!(&image_block(&bytes, *target, BLOCK_SIZE), original);
    }

    fs::remove_file(host).expect("remove journal host file");
    fs::remove_file(script).expect("remove journal script");
    for payload in payloads {
        fs::remove_file(payload).expect("remove journal payload");
    }
    fs::remove_file(image).expect("remove generated image");

    JournaledUpdateImage {
        bytes,
        targets,
        originals,
        replacements,
        block_size: BLOCK_SIZE,
        device_writes_per_filesystem_block: BLOCK_SIZE / DEVICE_BLOCK_SIZE,
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Dumpe2fsStats {
    inode_count: u64,
    block_count: u64,
    reserved_block_count: u64,
    overhead_clusters: u64,
    free_blocks: u64,
    free_inodes: u64,
    block_size: u64,
    fragment_size: u64,
}

struct JournaledUpdateImage {
    bytes: Vec<u8>,
    targets: Vec<u64>,
    originals: Vec<Vec<u8>>,
    replacements: Vec<Vec<u8>>,
    block_size: usize,
    device_writes_per_filesystem_block: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct SeenDirEntry {
    name: Vec<u8>,
    next_pos: Ext4DirPos,
}

struct LimitedDirSink {
    limit: usize,
    entries: Vec<SeenDirEntry>,
}

impl LimitedDirSink {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            entries: Vec::new(),
        }
    }
}

impl Ext4DirSink for LimitedDirSink {
    fn emit(&mut self, entry: Ext4DirEntryRef<'_>, next_pos: Ext4DirPos) -> Ext4Result<bool> {
        if self.entries.len() >= self.limit {
            return Ok(false);
        }
        self.entries.push(SeenDirEntry {
            name: Vec::from(entry.name_bytes()),
            next_pos,
        });
        Ok(true)
    }
}

fn read_dumpe2fs_stats(dumpe2fs: &Path, image: &Path) -> Dumpe2fsStats {
    let output = Command::new(dumpe2fs)
        .arg("-h")
        .arg(image)
        .output()
        .expect("run dumpe2fs");
    assert!(output.status.success(), "dumpe2fs failed");
    let output = String::from_utf8(output.stdout).expect("dumpe2fs output is UTF-8");

    Dumpe2fsStats {
        inode_count: parse_dumpe2fs_u64(&output, "Inode count"),
        block_count: parse_dumpe2fs_u64(&output, "Block count"),
        reserved_block_count: parse_dumpe2fs_u64(&output, "Reserved block count"),
        overhead_clusters: parse_dumpe2fs_u64(&output, "Overhead clusters"),
        free_blocks: parse_dumpe2fs_u64(&output, "Free blocks"),
        free_inodes: parse_dumpe2fs_u64(&output, "Free inodes"),
        block_size: parse_dumpe2fs_u64(&output, "Block size"),
        fragment_size: parse_dumpe2fs_u64(&output, "Fragment size"),
    }
}

fn parse_dumpe2fs_u64(output: &str, field: &str) -> u64 {
    output
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim() == field {
                Some(
                    value
                        .trim()
                        .parse::<u64>()
                        .unwrap_or_else(|_| panic!("parse dumpe2fs field {field}: {value}")),
                )
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("dumpe2fs field {field} not found"))
}

fn mark_image_needs_recovery(bytes: &mut [u8]) {
    const SUPERBLOCK_OFFSET: usize = 1024;
    const INCOMPAT_OFFSET: usize = SUPERBLOCK_OFFSET + 0x60;
    const CHECKSUM_OFFSET: usize = SUPERBLOCK_OFFSET + 0x3fc;
    const INCOMPAT_RECOVER: u32 = 0x0004;

    let incompat = u32::from_le_bytes(
        bytes[INCOMPAT_OFFSET..INCOMPAT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    bytes[INCOMPAT_OFFSET..INCOMPAT_OFFSET + 4]
        .copy_from_slice(&(incompat | INCOMPAT_RECOVER).to_le_bytes());
    let checksum = crc32c(u32::MAX, &bytes[SUPERBLOCK_OFFSET..CHECKSUM_OFFSET]);
    bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
}

fn image_needs_recovery(bytes: &[u8]) -> bool {
    const SUPERBLOCK_OFFSET: usize = 1024;
    const INCOMPAT_OFFSET: usize = SUPERBLOCK_OFFSET + 0x60;
    const INCOMPAT_RECOVER: u32 = 0x0004;

    let incompat = u32::from_le_bytes(
        bytes[INCOMPAT_OFFSET..INCOMPAT_OFFSET + 4]
            .try_into()
            .unwrap(),
    );
    incompat & INCOMPAT_RECOVER != 0
}

fn image_block(bytes: &[u8], block: u64, block_size: usize) -> Vec<u8> {
    let start = usize::try_from(block)
        .expect("block number fits usize")
        .checked_mul(block_size)
        .expect("block byte offset");
    let end = start.checked_add(block_size).expect("block byte end");
    bytes
        .get(start..end)
        .expect("image contains target block")
        .to_vec()
}

fn assert_legacy_logical_block(
    filesystem: &Ext4Filesystem,
    inode: &Ext4Inode,
    logical: u64,
    block_size: usize,
) {
    assert!(matches!(
        filesystem.map_blocks(inode, LogicalBlock::new(logical)),
        Ok(BlockMapping::Mapped { .. })
    ));
    let mut actual = vec![0; block_size];
    let offset = logical
        .checked_mul(block_size as u64)
        .expect("logical byte offset");
    let read = filesystem
        .read_at(inode, offset, &mut actual)
        .expect("read legacy indirect sample block");
    assert_eq!(read, block_size);
    assert!(
        actual
            .iter()
            .all(|byte| *byte == legacy_payload_byte(logical))
    );
}

fn legacy_payload_byte(logical: u64) -> u8 {
    logical.wrapping_mul(37).wrapping_add(1) as u8
}

fn legacy_inode_pointer(inode: &Ext4Inode, index: usize) -> u32 {
    let start = index.checked_mul(4).expect("legacy pointer offset");
    let end = start.checked_add(4).expect("legacy pointer end");
    u32::from_le_bytes(
        inode.raw_i_block()[start..end]
            .try_into()
            .expect("legacy inode pointer bytes"),
    )
}

fn image_block_entry_offset(block: u64, entry: u64, block_size: usize) -> usize {
    let block_offset = usize::try_from(block)
        .expect("block fits usize")
        .checked_mul(block_size)
        .expect("block offset");
    let entry_offset = usize::try_from(entry)
        .expect("entry fits usize")
        .checked_mul(4)
        .expect("entry offset");
    block_offset
        .checked_add(entry_offset)
        .expect("block entry offset")
}

fn put_le_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn legacy_indirect_inode_from_image(bytes: Vec<u8>, name: &str) -> (Ext4Filesystem, Ext4Inode) {
    let filesystem = Ext4Filesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount legacy indirect image");
    let root = filesystem.root_inode().expect("read root inode");
    let entry = filesystem
        .lookup(&root, name)
        .expect("lookup legacy indirect file")
        .expect("legacy indirect file exists");
    let inode = filesystem
        .load_inode_private(entry.inode())
        .expect("read legacy indirect inode");
    (filesystem, inode)
}

fn assert_image_blocks(bytes: &[u8], targets: &[u64], block_size: usize, expected: &[Vec<u8>]) {
    for (target, expected) in targets.iter().zip(expected) {
        assert_image_block(bytes, *target, block_size, expected);
    }
}

fn assert_image_block(bytes: &[u8], target: u64, block_size: usize, expected: &[u8]) {
    assert_eq!(image_block(bytes, target, block_size).as_slice(), expected);
}

fn assert_filesystem_blocks(filesystem: &Ext4Filesystem, targets: &[u64], expected: &[Vec<u8>]) {
    let block_size = filesystem.layout().block_size() as usize;
    let mut block = vec![0; block_size];
    for (target, expected) in targets.iter().zip(expected) {
        filesystem
            .read_blocks(FilesystemBlock::new(*target), 1, &mut block)
            .expect("read target block");
        assert_eq!(&block, expected);
    }
}

fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0x82f6_3b78;

    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (POLYNOMIAL & mask);
        }
    }
    crc
}

fn run_debugfs(debugfs: &Path, image: &Path, command: &str) {
    let status = Command::new(debugfs)
        .args(["-w", "-R", command])
        .arg(image)
        .status()
        .expect("run debugfs");
    assert!(status.success(), "debugfs command failed: {command}");
}

fn run_debugfs_script(debugfs: &Path, image: &Path, script: &Path) {
    let status = Command::new(debugfs)
        .arg("-w")
        .arg("-f")
        .arg(script)
        .arg(image)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run debugfs script");
    assert!(status.success(), "debugfs script failed");
}

fn run_e2fsck_optimize_directories(e2fsck: &Path, image: &Path) {
    let status = Command::new(e2fsck)
        .args(["-f", "-y", "-D"])
        .arg(image)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run e2fsck -D");
    assert!(
        matches!(status.code(), Some(0) | Some(1)),
        "e2fsck -D failed with status {status}"
    );
}

fn run_e2fsck_check(e2fsck: &Path, image: &Path) {
    let status = Command::new(e2fsck)
        .args(["-f", "-n"])
        .arg(image)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run e2fsck -n");
    assert_eq!(status.code(), Some(0), "e2fsck -n reported errors");
}

fn require_e2fsprogs(name: &str) -> PathBuf {
    find_e2fsprogs(name).unwrap_or_else(|| {
        panic!("{name} is required for kext4 Linux image interoperability tests")
    })
}

fn find_e2fsprogs(name: &str) -> Option<PathBuf> {
    [
        PathBuf::from(name),
        PathBuf::from("/opt/homebrew/opt/e2fsprogs/sbin").join(name),
        PathBuf::from("/usr/local/opt/e2fsprogs/sbin").join(name),
    ]
    .into_iter()
    .find(|path| Command::new(path).arg("-V").output().is_ok())
}

fn temporary_image_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("kext4-{label}-{}.img", std::process::id()))
}
