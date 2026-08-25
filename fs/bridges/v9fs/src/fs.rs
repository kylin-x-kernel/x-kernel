// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P filesystem adapter -- filesystem-level operations.

use alloc::{boxed::Box, string::String, sync::Arc};

use fs9p::{Session, Transport};
use ksync::{Mutex, MutexGuard};
use kvfs::{NodeFlags, StatFs, SuperBlock, SuperBlockOperations, VfsResult, path::MAX_NAME_LEN};

use super::inode::{Inode, inode_init_from_attr};

/// 9P filesystem implementation.
pub struct Fs9pFilesystem {
    inner: Mutex<Session>,
}

impl Fs9pFilesystem {
    /// Mount a 9P filesystem over an established 9P transport.
    pub fn mount(
        file_system_type: &'static kvfs::FileSystemType,
        superblock_flags: kvfs::SuperBlockFlags,
        transport: Box<dyn Transport>,
        mount_tag: String,
    ) -> VfsResult<Arc<SuperBlock>> {
        let mut session = Session::new(transport, mount_tag);
        session
            .negotiate()
            .map_err(|_| kvfs::VfsError::from(kerrno::LinuxError::EIO))?;

        let root_attr = session
            .getattr("/")
            .map_err(|_| kvfs::VfsError::from(kerrno::LinuxError::EIO))?;

        let fs = Arc::new(Self {
            inner: Mutex::new(session),
        });

        let root = Inode::new_dir(fs.clone(), Some("/".into())).into_dentry(
            inode_init_from_attr(&root_attr),
            NodeFlags::empty(),
            None,
            String::new(),
        );

        // The legacy adapter exposes inode number zero for every qid. Keep
        // those identities unhashed until qid-based `iget5` semantics exist;
        // hashing by zero would merge unrelated remote objects.
        Ok(SuperBlock::new_with_flags_and_private(
            file_system_type,
            &V9FS_SUPER_OPERATIONS,
            fs,
            superblock_flags,
            1,
            kvfs::MAX_LFS_FILESIZE,
            |_| root,
        ))
    }

    /// Lock the inner 9P session for sending requests.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Session> {
        self.inner.lock()
    }
}

struct V9fsSuperOperations;

static V9FS_SUPER_OPERATIONS: V9fsSuperOperations = V9fsSuperOperations;

impl SuperBlockOperations for V9fsSuperOperations {
    fn timestamp_limits(&self, super_block: &SuperBlock) -> kvfs::TimestampLimits {
        let fs = super_block
            .private::<Arc<Fs9pFilesystem>>()
            .expect("9P superblock initialization must install its session before capabilities");
        kvfs::TimestampLimits::new(1, 0, fs.lock().timestamp_max_seconds())
    }

    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
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
        })
    }

    fn sync_fs(&self, _super_block: &SuperBlock) -> VfsResult<()> {
        // 9P writes are synchronous through virtio — no explicit flush needed.
        Ok(())
    }
}
