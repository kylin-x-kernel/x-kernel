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
use kvfs::{SimpleFs, SuperBlock, SuperBlockFlags};
pub use nodes::pts::Ptmx;

const DEVFS_MAGIC: u32 = 0x0102_1994;

/// Creates a devfs superblock for device access.
pub fn new_devfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags(
        "devfs".into(),
        DEVFS_MAGIC,
        superblock_flags,
        root::builder,
    )
}

/// Capture a snapshot of the firmware device tree blob.
pub fn capture_firmware_dtb_snapshot() {
    nodes::dtb::capture_snapshot();
}

#[cfg(feature = "dev-log")]
pub use nodes::log::bind_dev_log;
