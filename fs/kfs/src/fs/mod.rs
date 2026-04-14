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
use fs_ng_vfs::{Filesystem, Location, VfsError, VfsResult};
use kdriver::BlockDevice;
#[cfg(feature = "fs9p")]
use kdriver::Virtio9pDevice;

/// Create the default filesystem instance for the given block device.
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

pub(crate) fn range_shift(
    _location: &Location,
    _offset: u64,
    _len: u64,
    _insert: bool,
) -> VfsResult<()> {
    #[cfg(feature = "ext4")]
    {
        return ext4::Ext4Filesystem::range_shift(_location, _offset, _len, _insert);
    }

    #[allow(unreachable_code)]
    Err(VfsError::Unsupported)
}

/// Create the default 9p filesystem instance for the given virtio-9p device.
#[cfg(feature = "fs9p")]
pub fn new_9p_filesystem(_dev: Virtio9pDevice) -> VfsResult<Filesystem> {
    fs9p::Fs9pFilesystem::new(_dev)
}
