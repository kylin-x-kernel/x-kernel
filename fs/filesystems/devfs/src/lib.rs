// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device filesystem (devfs) implementation.

#![no_std]
#![feature(bstr)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod device_file;
mod nodes;
mod root;

use alloc::sync::Arc;

pub use device_file::DeviceFile;
pub(crate) use device_file::{add_device_entry, device_dentry};
use kvfs::{SimpleFs, StatFsFlags, SuperBlock};
pub use nodes::pts::Ptmx;

const DEV_MOUNT_FLAGS: StatFsFlags = StatFsFlags::NOSUID
    .union(StatFsFlags::NODEV)
    .union(StatFsFlags::NOEXEC)
    .union(StatFsFlags::RELATIME);
const DEVFS_MAGIC: u32 = 0x0102_1994;

/// Creates a devfs superblock for device access.
pub fn new_devfs() -> Arc<SuperBlock> {
    SimpleFs::new_with_flags("devfs".into(), DEVFS_MAGIC, DEV_MOUNT_FLAGS, root::builder)
}

/// Capture a snapshot of the firmware device tree blob.
pub fn capture_firmware_dtb_snapshot() {
    nodes::dtb::capture_snapshot();
}

#[cfg(feature = "dev-log")]
pub use nodes::log::bind_dev_log;
