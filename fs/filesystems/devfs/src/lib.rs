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

pub use device_file::DeviceFile;
use kvfs::{Filesystem, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME};
use kvfs_simple::SimpleFs;
pub use nodes::pts::Ptmx;

const DEV_MOUNT_FLAGS: u32 = ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME;

/// Create a new devfs filesystem for device access.
pub fn new_devfs() -> Filesystem {
    SimpleFs::new_with_flags("devfs".into(), 0x01021994, DEV_MOUNT_FLAGS, root::builder)
}

/// Capture a snapshot of the firmware device tree blob.
pub fn capture_firmware_dtb_snapshot() {
    nodes::dtb::capture_snapshot();
}

#[cfg(feature = "dev-log")]
pub use nodes::log::bind_dev_log;
