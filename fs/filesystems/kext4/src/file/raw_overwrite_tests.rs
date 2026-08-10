// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec, vec::Vec};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};

use crate::{Ext4Error, Ext4Filesystem, UnsupportedKind};

const DEVICE_BLOCK_SIZE: usize = 512;

struct WritableImageDevice {
    bytes: Mutex<Vec<u8>>,
}

impl WritableImageDevice {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Mutex::new(bytes),
        }
    }
}

impl Device for WritableImageDevice {
    fn name(&self) -> &str {
        "kext4-raw-overwrite-test-image"
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

#[test]
fn overwrites_existing_linux_file_extent() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("overwrite");
    create_image(&mke2fs, &image);

    let hello = temporary_image_path("overwrite-host");
    fs::write(&hello, b"hello from linux ext4\n").expect("write host hello file");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /hello.txt", hello.display()),
    );

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let device = Arc::new(WritableImageDevice::new(bytes));
    let filesystem = Ext4Filesystem::mount(device.clone()).expect("mount writable test image");
    let root = filesystem.root_inode().expect("read root inode");
    let hello_entry = filesystem
        .lookup(&root, "hello.txt")
        .expect("lookup hello")
        .expect("hello exists");
    let hello_inode = filesystem
        .load_inode_private(hello_entry.inode())
        .expect("read hello inode");
    let old_ctime = hello_inode.ctime();
    let old_mtime = hello_inode.mtime();

    assert_eq!(
        filesystem
            .raw_overwrite_allocated_data_unjournaled(&hello_inode, 6, b"KExt4")
            .expect("overwrite existing extent"),
        5
    );

    let filesystem = Ext4Filesystem::mount(device).expect("remount overwritten image");
    let root = filesystem
        .root_inode()
        .expect("read root inode after overwrite");
    let hello_entry = filesystem
        .lookup(&root, "hello.txt")
        .expect("lookup overwritten hello")
        .expect("hello exists after overwrite");
    let hello_inode = filesystem
        .load_inode_private(hello_entry.inode())
        .expect("read overwritten hello inode");
    let mut output = vec![0; hello_inode.size() as usize];
    let read = filesystem
        .read_at(&hello_inode, 0, &mut output)
        .expect("read overwritten hello");

    assert_eq!(read, output.len());
    assert_eq!(&output, b"hello KExt4linux ext4\n");
    assert_eq!(hello_inode.size(), b"hello from linux ext4\n".len() as u64);
    assert_eq!(hello_inode.mtime(), old_mtime);
    assert_eq!(hello_inode.ctime(), old_ctime);
    fs::remove_file(hello).expect("remove host hello file");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn overwrite_past_eof_is_noop_and_does_not_update_timestamps() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("overwrite-eof");
    create_image(&mke2fs, &image);

    let hello = temporary_image_path("overwrite-eof-host");
    fs::write(&hello, b"hello from linux ext4\n").expect("write host hello file");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /hello.txt", hello.display()),
    );

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let device = Arc::new(WritableImageDevice::new(bytes));
    let filesystem = Ext4Filesystem::mount(device.clone()).expect("mount writable test image");
    let root = filesystem.root_inode().expect("read root inode");
    let hello_entry = filesystem
        .lookup(&root, "hello.txt")
        .expect("lookup hello")
        .expect("hello exists");
    let hello_inode = filesystem
        .load_inode_private(hello_entry.inode())
        .expect("read hello inode");
    let old_ctime = hello_inode.ctime();
    let old_mtime = hello_inode.mtime();

    assert_eq!(
        filesystem
            .raw_overwrite_allocated_data_unjournaled(&hello_inode, hello_inode.size(), b"ignored")
            .expect("overwrite at EOF is a no-op"),
        0
    );

    let filesystem = Ext4Filesystem::mount(device).expect("remount after EOF no-op");
    let root = filesystem
        .root_inode()
        .expect("read root inode after EOF no-op");
    let hello_entry = filesystem
        .lookup(&root, "hello.txt")
        .expect("lookup hello after EOF no-op")
        .expect("hello exists after EOF no-op");
    let hello_inode = filesystem
        .load_inode_private(hello_entry.inode())
        .expect("read hello inode after EOF no-op");
    assert_eq!(hello_inode.size(), b"hello from linux ext4\n".len() as u64);
    assert_eq!(hello_inode.ctime(), old_ctime);
    assert_eq!(hello_inode.mtime(), old_mtime);

    fs::remove_file(hello).expect("remove host hello file");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn overwrite_hole_returns_unallocated_write() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("overwrite-hole");
    create_image(&mke2fs, &image);

    let sparse = temporary_image_path("overwrite-hole-host");
    let mut sparse_file = fs::File::create(&sparse).expect("create sparse host file");
    sparse_file
        .seek(SeekFrom::Start(8192))
        .expect("seek sparse host file");
    sparse_file.write_all(b"tail").expect("write sparse tail");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /sparse.bin", sparse.display()),
    );
    run_debugfs(&debugfs, &image, "punch /sparse.bin 0 1");

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem =
        Ext4Filesystem::mount(Arc::new(WritableImageDevice::new(bytes))).expect("mount image");
    let root = filesystem.root_inode().expect("read root inode");
    let sparse_entry = filesystem
        .lookup(&root, "sparse.bin")
        .expect("lookup sparse")
        .expect("sparse exists");
    let sparse_inode = filesystem
        .load_inode_private(sparse_entry.inode())
        .expect("read sparse inode");

    assert_eq!(
        filesystem.raw_overwrite_allocated_data_unjournaled(&sparse_inode, 0, b"nope"),
        Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite))
    );

    fs::remove_file(sparse).expect("remove sparse host file");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn raw_overwrite_cross_hole_rejects_range_without_prefix_write() {
    let mke2fs = require_e2fsprogs("mke2fs");
    let debugfs = require_e2fsprogs("debugfs");
    let image = temporary_image_path("overwrite-cross-hole");
    create_image(&mke2fs, &image);

    let sparse = temporary_image_path("overwrite-cross-hole-host");
    fs::write(&sparse, vec![b'a'; 8192]).expect("write host file");
    run_debugfs(
        &debugfs,
        &image,
        &format!("write {} /sparse.bin", sparse.display()),
    );
    run_debugfs(&debugfs, &image, "punch /sparse.bin 1 1");

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let device = Arc::new(WritableImageDevice::new(bytes));
    let filesystem = Ext4Filesystem::mount(device.clone()).expect("mount image");
    let root = filesystem.root_inode().expect("read root inode");
    let sparse_entry = filesystem
        .lookup(&root, "sparse.bin")
        .expect("lookup sparse")
        .expect("sparse exists");
    let sparse_inode = filesystem
        .load_inode_private(sparse_entry.inode())
        .expect("read sparse inode");
    let offset = u64::from(filesystem.layout().block_size()) - 2;
    let mut before = vec![0; 16];
    filesystem
        .read_at(&sparse_inode, offset, &mut before)
        .expect("read bytes across hole before overwrite");

    assert_eq!(
        filesystem.raw_overwrite_allocated_data_unjournaled(&sparse_inode, offset, b"changed!"),
        Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite))
    );

    let filesystem = Ext4Filesystem::mount(device).expect("remount after failed raw overwrite");
    let sparse_inode = filesystem
        .load_inode_private(sparse_entry.inode())
        .expect("read sparse inode after failed raw overwrite");
    let mut after = vec![0; before.len()];
    filesystem
        .read_at(&sparse_inode, offset, &mut after)
        .expect("read bytes across hole after failed overwrite");
    assert_eq!(after, before);

    fs::remove_file(sparse).expect("remove sparse host file");
    fs::remove_file(image).expect("remove generated image");
}

fn block_start(block_id: u64) -> Result<usize, DriverError> {
    usize::try_from(block_id)
        .map_err(|_| DriverError::InvalidInput)?
        .checked_mul(DEVICE_BLOCK_SIZE)
        .ok_or(DriverError::InvalidInput)
}

fn create_image(mke2fs: &Path, image: &Path) {
    let file = fs::File::create(image).expect("create ext4 image");
    file.set_len(256 * 1024 * 1024).expect("size ext4 image");
    let features = "has_journal,extent,filetype,64bit,flex_bg,metadata_csum,dir_index,\
                    ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
                    ^verity,^casefold,^mmp";
    let status = Command::new(mke2fs)
        .args(["-q", "-t", "ext4", "-F", "-b", "4096", "-I", "256"])
        .arg("-O")
        .arg(features)
        .arg(image)
        .status()
        .expect("run mke2fs");
    assert!(status.success(), "mke2fs failed");
}

fn run_debugfs(debugfs: &Path, image: &Path, command: &str) {
    let status = Command::new(debugfs)
        .args(["-w", "-R", command])
        .arg(image)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run debugfs");
    assert!(status.success(), "debugfs command failed: {command}");
}

fn require_e2fsprogs(name: &str) -> PathBuf {
    find_e2fsprogs(name)
        .unwrap_or_else(|| panic!("{name} is required for kext4 raw overwrite tests"))
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
    std::env::temp_dir().join(format!("kext4-raw-{label}-{}.img", std::process::id()))
}
