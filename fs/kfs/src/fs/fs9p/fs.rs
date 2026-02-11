// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtio-9p filesystem adapter.
use alloc::{boxed::Box, string::ToString, string::String, sync::Arc};
use core::cell::OnceCell;

use fs_ng_vfs::{
    DirEntry, DirNode, Filesystem, FilesystemOps, Reference, StatFs, VfsResult,
    path::MAX_NAME_LEN,
};
use fs9p::Session;
use kdriver::Virtio9pDevice;
use kspin::{SpinNoPreempt as Mutex, SpinNoPreemptGuard as MutexGuard};

use super::{Inode, util::into_vfs_err};

struct Virtio9pTransport {
    dev: Mutex<Virtio9pDevice>,
}

impl Virtio9pTransport {
    fn new(dev: Virtio9pDevice) -> Self {
        Self {
            dev: Mutex::new(dev),
        }
    }
}

impl fs9p::Transport for Virtio9pTransport {
    fn request(&self, req: &[u8], resp: &mut [u8]) -> Result<usize, String> {
        let mut dev = self.dev.lock();
        dev.request(req, resp)
            .map_err(|err| alloc::format!("{err:?}"))
    }
}

pub(crate) struct Fs9pState {
    pub session: Session,
}

/// Virtio-9p filesystem implementation.
pub struct Fs9pFilesystem {
    inner: Mutex<Fs9pState>,
    root_dir: OnceCell<DirEntry>,
}

impl Fs9pFilesystem {
    /// Create a new 9p filesystem instance backed by a virtio-9p device.
    pub fn new(dev: Virtio9pDevice) -> VfsResult<Filesystem> {
        let mount_tag = dev.mount_tag().to_string();
        let transport = Box::new(Virtio9pTransport::new(dev));
        let mut session = Session::new(transport, mount_tag);
        session.negotiate().map_err(into_vfs_err)?;

        let root_info = session.lookup_path("/").ok();
        let root_ino = root_info
            .as_ref()
            .map(|info| info.qid_path)
            .unwrap_or(1);

        let fs = Arc::new(Self {
            inner: Mutex::new(Fs9pState { session }),
            root_dir: OnceCell::new(),
        });
        let _ = fs.root_dir.set(DirEntry::new_dir(
            |this| {
                DirNode::new(Inode::new(
                    fs.clone(),
                    root_ino,
                    true,
                    false,
                    Some(this),
                    String::from("/"),
                ))
            },
            Reference::root(),
        ));
        Ok(Filesystem::new(fs))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Fs9pState> {
        self.inner.lock()
    }
}

unsafe impl Send for Fs9pFilesystem {}
unsafe impl Sync for Fs9pFilesystem {}

impl FilesystemOps for Fs9pFilesystem {
    fn name(&self) -> &str {
        "9p"
    }

    fn root_dir(&self) -> DirEntry {
        self.root_dir.get().unwrap().clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: 0x9fa0,
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
}
