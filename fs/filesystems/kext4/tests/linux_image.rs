// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use block::{BlockDevice, Device, DeviceKind, DriverError, DriverResult};
use kext4::{Ext4Error, FilesystemBlock, ReadOnlyFilesystem};

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

impl Device for ImageDevice {
    fn name(&self) -> &str {
        "kext4-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDevice for ImageDevice {
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
    let Some(mke2fs) = find_e2fsprogs("mke2fs") else {
        eprintln!("skipping Linux image test: mke2fs is not installed");
        return;
    };
    let image = temporary_image_path("valid");
    create_image(&mke2fs, &image);

    let bytes = fs::read(&image).expect("read generated ext4 image");
    let filesystem = ReadOnlyFilesystem::mount(Arc::new(ImageDevice::new(bytes)))
        .expect("mount generated Linux ext4 image");
    assert_eq!(filesystem.layout().block_size(), 4096);
    assert_eq!(
        filesystem.groups().len(),
        filesystem.layout().group_count() as usize
    );

    let mut block = vec![0; filesystem.layout().block_size() as usize];
    filesystem
        .read_blocks(FilesystemBlock::new(0), 1, &mut block)
        .expect("read first filesystem block");
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_corrupt_superblock_checksum() {
    let Some(mke2fs) = find_e2fsprogs("mke2fs") else {
        eprintln!("skipping Linux image test: mke2fs is not installed");
        return;
    };
    let image = temporary_image_path("bad-super");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes[1024 + 0x10] ^= 1;
    let error = match ReadOnlyFilesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("corrupt superblock unexpectedly mounted"),
        Err(error) => error,
    };
    assert!(matches!(error, Ext4Error::ChecksumMismatch { .. }));
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_corrupt_group_descriptor_checksum() {
    let Some(mke2fs) = find_e2fsprogs("mke2fs") else {
        eprintln!("skipping Linux image test: mke2fs is not installed");
        return;
    };
    let image = temporary_image_path("bad-group");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes[4096 + 12] ^= 1;
    let error = match ReadOnlyFilesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("corrupt group descriptor unexpectedly mounted"),
        Err(error) => error,
    };
    assert!(matches!(error, Ext4Error::ChecksumMismatch { .. }));
    fs::remove_file(image).expect("remove generated image");
}

#[test]
fn rejects_device_shorter_than_superblock_geometry() {
    let Some(mke2fs) = find_e2fsprogs("mke2fs") else {
        eprintln!("skipping Linux image test: mke2fs is not installed");
        return;
    };
    let image = temporary_image_path("short-device");
    create_image(&mke2fs, &image);

    let mut bytes = fs::read(&image).expect("read generated ext4 image");
    bytes.truncate(bytes.len() / 2);
    let error = match ReadOnlyFilesystem::mount(Arc::new(ImageDevice::new(bytes))) {
        Ok(_) => panic!("truncated block device unexpectedly mounted"),
        Err(error) => error,
    };
    assert_eq!(error, Ext4Error::OutOfBounds);
    fs::remove_file(image).expect("remove generated image");
}

fn create_image(mke2fs: &Path, image: &Path) {
    let file = fs::File::create(image).expect("create ext4 image");
    file.set_len(256 * 1024 * 1024).expect("size ext4 image");
    let status = Command::new(mke2fs)
        .args([
            "-q",
            "-t",
            "ext4",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "has_journal,extent,filetype,64bit,flex_bg,metadata_csum,dir_index,\
             ^metadata_csum_seed,^orphan_file,^fast_commit,^bigalloc,^inline_data,^encrypt,\
             ^verity,^casefold,^mmp",
        ])
        .arg(image)
        .status()
        .expect("run mke2fs");
    assert!(status.success(), "mke2fs failed");
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
