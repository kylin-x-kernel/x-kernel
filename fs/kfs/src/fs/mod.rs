// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem backends and selection helpers.
#[cfg(feature = "fat")]
mod fat;

#[cfg(feature = "ext4")]
mod ext4;

#[cfg(feature = "fs9p")]
pub(crate) mod fs9p;

use cfg_if::cfg_if;
use fs_ng_vfs::{Filesystem, VfsResult};
use kdriver::{BlockDevice, Virtio9pDevice};

/// Create the default filesystem instance for the given block device.
#[cfg(feature = "ext4")]
pub fn new_default(_dev: BlockDevice) -> VfsResult<Filesystem> {
    cfg_if! {
        if #[cfg(feature = "ext4")] {
            ext4::Ext4Filesystem::new(_dev)
        } else if #[cfg(feature = "fat")] {
            Ok(fat::FatFilesystem::new(_dev))
        } else {
            panic!("No filesystem feature enabled");
        }
    }
}

/// Create the default 9p filesystem instance for the given virtio-9p device.
#[cfg(feature = "fs9p")]
pub fn new_9p_filesystem(_dev: Virtio9pDevice) -> VfsResult<Filesystem> {
    #[cfg(feature = "fs9p")]
    {
        fs9p::Fs9pFilesystem::new(_dev)
    }
    #[cfg(not(feature = "fs9p"))]
    {
        panic!("No virtio_9p feature enabled");
    }
}