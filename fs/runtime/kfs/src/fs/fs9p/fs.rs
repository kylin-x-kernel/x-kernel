// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P filesystem adapter — filesystem-level operations.

use alloc::{boxed::Box, sync::Arc};

use fs9p::Session;
use kclass::{ClassDevice, Virtio9pDeviceImpl};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    DirEntry, DirNode, Filesystem, Reference, StatFs, SuperBlockOperations, VfsResult,
    path::MAX_NAME_LEN,
};

use super::{VirtioTransport, inode::Inode};

/// 9P filesystem implementation backed by a virtio-9p device.
pub struct Fs9pFilesystem {
    inner: Mutex<Session>,
    root_dir: Mutex<Option<DirEntry>>,
}

impl Fs9pFilesystem {
    /// Create a new 9P filesystem instance from a virtio-9p device.
    pub fn new(dev: ClassDevice<Virtio9pDeviceImpl>) -> VfsResult<Filesystem> {
        let mount_tag = dev.mount_tag().into();
        let transport = Box::new(VirtioTransport(Mutex::new(dev)));
        let mut session = Session::new(transport, mount_tag);
        session
            .negotiate()
            .map_err(|_| kvfs::VfsError::from(kerrno::LinuxError::EIO))?;

        let fs = Arc::new(Self {
            inner: Mutex::new(session),
            root_dir: Mutex::new(None),
        });

        *fs.root_dir.lock() = Some(DirEntry::new_dir(
            |this| DirNode::new(Inode::new_dir(fs.clone(), Some(this), Some("/".into()))),
            Reference::root(),
        ));

        Ok(Filesystem::new(fs))
    }

    /// Lock the inner 9P session for sending requests.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Session> {
        self.inner.lock()
    }
}

impl SuperBlockOperations for Fs9pFilesystem {
    fn name(&self) -> &str {
        "9p"
    }

    fn root_dentry(&self) -> DirEntry {
        self.root_dir.lock().clone().unwrap()
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        // 9P does not expose filesystem-wide statistics in a standard way.
        // Return a minimal StatFs with zeroed fields.
        Ok(StatFs {
            fs_type: 0x01021997, // V9FS_MAGIC
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 0,
            free_file_count: 0,
            name_length: MAX_NAME_LEN as _,
            fragment_size: 0,
            mount_flags: 0,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        // 9P writes are synchronous through virtio — no explicit flush needed.
        Ok(())
    }
}
