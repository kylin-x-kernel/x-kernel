// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P filesystem mount and superblock operations.

use alloc::{boxed::Box, string::String, sync::Arc};

use kvfs::{
    Dentry, FileSystemType, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations, VfsError,
    VfsResult, libfs::simple_statfs,
};
use p9::{P9Client, P9Transport};

use super::inode::{V9fsInode, inode_init_from_attr};

const V9FS_MAGIC: u32 = 0x0102_1997;

/// Per-mount v9fs state, corresponding to Linux `v9fs_session_info`.
struct V9fsSession {
    client: Arc<P9Client>,
}

/// Mounts a 9P2000.L filesystem over an already connected transport.
///
/// Transport discovery remains outside v9fs. The returned superblock owns the
/// negotiated client through its private session state, while inodes and open
/// files retain only the fids needed for their own lifetimes.
///
/// # Errors
///
/// Returns an error when version negotiation, root attachment, root metadata
/// validation, or superblock construction fails.
pub fn mount(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
    transport: Box<dyn P9Transport>,
) -> VfsResult<Arc<SuperBlock>> {
    let client = P9Client::new(transport);
    let root_fid = client.negotiate()?;
    let root_attr = root_fid.getattr()?;
    V9fsInode::validate_attr(&root_fid, &root_attr)?;
    if !root_attr.qid.is_dir() {
        return Err(VfsError::NotADirectory);
    }

    let block_size = u64::from(client.max_data_size()).next_power_of_two();
    let session = V9fsSession { client };
    Ok(SuperBlock::new_with_flags_and_private(
        file_system_type,
        &V9FS_SUPER_OPERATIONS,
        session,
        superblock_flags,
        block_size,
        kvfs::MAX_LFS_FILESIZE,
        move |super_block| {
            let inode = V9fsInode::new(root_fid)
                .into_vfs_inode(inode_init_from_attr(&root_attr, super_block.block_size()));
            Dentry::new_dir_from_inode(inode, None, String::new())
        },
    ))
}

struct V9fsSuperOperations;

static V9FS_SUPER_OPERATIONS: V9fsSuperOperations = V9fsSuperOperations;

impl SuperBlockOperations for V9fsSuperOperations {
    fn timestamp_limits(&self, super_block: &SuperBlock) -> kvfs::TimestampLimits {
        let session = super_block
            .private::<V9fsSession>()
            .expect("9P superblock initialization must install its session");
        kvfs::TimestampLimits::new(1, 0, session.client.timestamp_max_seconds())
    }

    fn statfs(&self, super_block: &SuperBlock) -> VfsResult<StatFs> {
        let root = super_block.root_dir();
        let inode = root.downcast::<V9fsInode>()?;
        let stats = match inode.statfs() {
            Ok(stats) => stats,
            Err(error) if error.canonicalize() == VfsError::Unsupported => {
                return Ok(simple_statfs(V9FS_MAGIC));
            }
            Err(error) => return Err(error),
        };
        Ok(StatFs {
            fs_type: stats.fs_type,
            block_size: stats.block_size,
            blocks: stats.blocks,
            blocks_free: stats.blocks_free,
            blocks_available: stats.blocks_available,
            file_count: stats.file_count,
            free_file_count: stats.free_file_count,
            name_length: stats.name_length,
            fragment_size: 0,
        })
    }

    fn sync_fs(&self, _super_block: &SuperBlock) -> VfsResult<()> {
        // Like Linux v9fs, this layer has no superblock-owned dirty state;
        // durability for an opened object is expressed by per-fid TFSYNC.
        Ok(())
    }
}
