// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel filesystem initialization and high-level APIs.
#![cfg_attr(all(not(test), not(doc)), no_std)]
#![feature(likely_unlikely)]
#![allow(dead_code, unused_imports, rustdoc::broken_intra_doc_links)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

#[macro_use]
extern crate log;

#[cfg(feature = "fs9p")]
use alloc::borrow::ToOwned;

mod test_fs_context;

use alloc::{sync::Arc, vec::Vec};

use kclass::block_devices;
#[cfg(feature = "fs9p")]
use kclass::{Virtio9pDevice as _, virtio_9p_devices};
use kdevice::{DeviceId as KDeviceId, subscribe_device_removed};
use ksync::Mutex;
#[cfg(feature = "fs9p")]
use kvfs::{
    NodePermission,
    path::{Path, PathBuf},
};

static FS_BACKING_DEVICES: Mutex<Vec<KDeviceId>> = Mutex::new(Vec::new());

#[cfg(feature = "fat")]
mod disk;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod fs;

mod virtual_filesystems;

mod highlevel;
pub use highlevel::*;
pub use virtual_filesystems::{VirtualFsMounts, mount_virtual_filesystems};

/// Initialize the filesystem subsystem and mount the root filesystem.
pub fn init_filesystems() {
    info!("Initialize filesystem subsystem...");

    let mut block_devs = block_devices();
    let handle = {
        #[cfg(feature = "crosvm")]
        {
            assert!(block_devs.len() >= 2, "Less than two block devices found!");
            block_devs.remove(1)
        }
        #[cfg(not(feature = "crosvm"))]
        {
            block_devs.pop().expect("No block device found!")
        }
    };

    let backing_id = handle.id();
    subscribe_fs_backing_unregister(backing_id, "block");
    info!(
        "  use block device 0: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );

    let fs = match fs::new_default(handle) {
        Ok(fs) => fs,
        Err(e) => {
            error!("Failed to mount root filesystem: {e:?}");
            panic!("VFS: Unable to mount root fs");
        }
    };
    info!("  filesystem type: {:?}", fs.name());

    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_context = FsContext::new(mp.root_location());
    ROOT_FS_CONTEXT.call_once(|| root_context.clone());
    KERNEL_FS_CONTEXT.call_once(|| Arc::new(Mutex::new(root_context)));
}

#[cfg(feature = "fs9p")]
pub fn mount_9pfilesystems(mount_path: &str) {
    let mut virtio_9p_devs = virtio_9p_devices();
    let handle = virtio_9p_devs.pop().expect("No virtio-9p device found!");
    let backing_id = handle.id();
    subscribe_fs_backing_unregister(backing_id, "virtio-9p");
    info!("Mount 9P filesystem...");
    info!("  use virtio-9p device: {:?}", handle.name());
    info!("  mount tag: {:?}", handle.mount_tag());

    let fs = fs::new_9p_filesystem(handle).expect("Failed to initialize filesystem");
    info!("  filesystem type: {:?}", fs.name());
    let mut fs_ctx = kernel_fs_context().lock();
    ensure_mount_path(&mut fs_ctx, mount_path).expect("Failed to prepare 9P mount path");
    fs_ctx
        .resolve(mount_path)
        .and_then(|loc| loc.mount(&fs).map(|_| ()))
        .expect("Failed to mount 9P filesystem");
    info!("  mounted at: {:?}", mount_path);
}

fn subscribe_fs_backing_unregister(id: KDeviceId, label: &'static str) {
    FS_BACKING_DEVICES.lock().push(id);
    subscribe_device_removed(Arc::new(move |removed_id| {
        if removed_id != id {
            return;
        }
        let mut devices = FS_BACKING_DEVICES.lock();
        if let Some(pos) = devices.iter().position(|device_id| *device_id == id) {
            devices.swap_remove(pos);
            warn!(
                "filesystem: mounted {} backing device {:?} was removed; mounted filesystem is \
                 now stale",
                label, id
            );
        }
    }));
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
