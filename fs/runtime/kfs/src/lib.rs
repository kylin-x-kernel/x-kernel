// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel filesystem initialization and high-level APIs.
#![cfg_attr(all(not(test), not(doc)), no_std)]
#![allow(dead_code, unused_imports, rustdoc::broken_intra_doc_links)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

#[cfg(feature = "fs9p")]
use alloc::borrow::ToOwned;
use alloc::sync::Arc;

mod test_path_resolver;
mod test_working_context;

#[cfg(feature = "fs9p")]
use kdriver::Virtio9pDevice;
use kdriver::{BlockDevice, DeviceContainer, prelude::*};
use ksync::Mutex;
#[cfg(feature = "fs9p")]
use kvfs::{
    NodePermission,
    path::{Path, PathBuf},
};

#[cfg(feature = "fat")]
mod disk;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod fs;

// New refactored components
mod fs_operations;
mod path_resolver;
mod virtual_filesystems;
mod working_context;

mod highlevel;
// Export new components (FsOperations for advanced use)
pub use fs_operations::FsOperations;
pub use highlevel::*;
pub use path_resolver::PathResolver;
pub use virtual_filesystems::{VirtualFsMounts, mount_virtual_filesystems};
pub use working_context::WorkingContext;

/// Initialize the filesystem subsystem and mount the root filesystem.
pub fn init_filesystems(mut block_devs: DeviceContainer<BlockDevice>) {
    info!("Initialize filesystem subsystem...");

    let dev = {
        #[cfg(feature = "crosvm")]
        {
            // must have two block devices: secure and non-secure
            // we only use the second blk
            block_devs
                .take_nth(1)
                .expect("Less than two block devices found!")
        }
        #[cfg(not(feature = "crosvm"))]
        {
            block_devs.take_one().expect("No block device found!")
        }
    };
    info!("  use block device 0: {:?}", dev.name());

    let fs = fs::new_default(dev).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());

    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_context = FsContext::new(mp.root_location());
    ROOT_FS_CONTEXT.call_once(|| root_context.clone());
    KERNEL_FS_CONTEXT.call_once(|| Arc::new(Mutex::new(root_context)));
}

#[cfg(feature = "fs9p")]
pub fn mount_9pfilesystems(mut virtio_9p_devs: DeviceContainer<Virtio9pDevice>, mount_path: &str) {
    let dev_9p = virtio_9p_devs
        .take_one()
        .expect("No virtio-9p device found!");
    info!("Mount 9P filesystem...");
    info!("  use virtio-9p device: {:?}", dev_9p.name());
    info!("  mount tag: {:?}", dev_9p.mount_tag());

    let fs = fs::new_9p_filesystem(dev_9p).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());
    let mut fs_ctx = kernel_fs_context().lock();
    ensure_mount_path(&mut fs_ctx, mount_path).expect("Failed to prepare 9P mount path");
    fs_ctx
        .resolve(mount_path)
        .and_then(|loc| loc.mount(&fs).map(|_| ()))
        .expect("Failed to mount 9P filesystem");
    info!("  mounted at: {:?}", mount_path);
}

#[cfg(feature = "fs9p")]
fn ensure_mount_path(fs: &mut FsContext, mount_path: &str) -> kvfs::VfsResult<()> {
    const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);

    let mut path = PathBuf::new();
    for comp in Path::new(mount_path).components() {
        path.push(comp.as_str());
        if fs.resolve(&path).is_err() {
            fs.create_dir(&path, DIR_PERMISSION)?;
        }
    }
    Ok(())
}
