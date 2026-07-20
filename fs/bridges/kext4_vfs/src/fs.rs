// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KExt4 superblock operations.

use alloc::{string::String, sync::Arc};

use block::BlockDevice;
use kclass::{BlockDeviceImpl as KBlockDevice, ClassDevice};
use kext4::{Ext4Error, Ext4Filesystem as KExt4Core, Ext4Inode, InodeNumber, SymlinkStorage};
use ksync::{Mutex, MutexGuard};
use kvfs::{
    Dentry, DeviceId, InodeCache, Metadata, NodeFlags, NodeType, StatFs, StatFsFlags, SuperBlock,
    SuperBlockOperations, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult, default_evict_inode,
};

use super::{
    inode::Inode,
    util::{
        current_ext4_timestamp, ext4_device_id_to_vfs, ext4_timestamp_to_duration,
        inode_kind_to_vfs, into_vfs_err,
    },
};

const EXT4_ROOT_INO: u32 = 2;
const EXT4_LOGICAL_BLOCK_COUNT: u64 = u32::MAX as u64 + 1;

fn extent_max_file_size_bytes(block_size: u32) -> u64 {
    EXT4_LOGICAL_BLOCK_COUNT * u64::from(block_size)
}

/// Ext4 filesystem implementation backed by the checked KExt4 core.
pub struct Ext4Filesystem {
    inner: Mutex<KExt4Core>,
    block_size: u32,
    delalloc_reserved_blocks: Mutex<u64>,
    inode_cache: InodeCache,
}

impl Ext4Filesystem {
    /// Mount a KExt4 filesystem backed by a block device.
    pub fn mount_bdev(dev: ClassDevice<KBlockDevice>) -> VfsResult<Arc<SuperBlock>> {
        let device: Arc<dyn BlockDevice> = Arc::new(dev);
        let core = match KExt4Core::mount(device.clone()) {
            Ok(core) => core,
            Err(Ext4Error::NeedsRecovery) => {
                if let Err(err) = KExt4Core::recover(device.clone()) {
                    error!("KExt4 recovery failed: {err:?}");
                    return Err(into_vfs_err(err));
                }
                match KExt4Core::mount(device) {
                    Ok(core) => core,
                    Err(err) => {
                        error!("KExt4 mount after journal recovery failed: {err:?}");
                        return Err(into_vfs_err(err));
                    }
                }
            }
            Err(err) => {
                error!("KExt4 core mount failed: {err:?}");
                return Err(into_vfs_err(err));
            }
        };

        let block_size = core.layout().block_size();
        let fs = Arc::new(Self {
            inner: Mutex::new(core),
            block_size,
            delalloc_reserved_blocks: Mutex::new(0),
            inode_cache: InodeCache::new(),
        });
        let root_inode = fs
            .load_inode(InodeNumber::new(EXT4_ROOT_INO))
            .inspect_err(|err| error!("KExt4 root inode load failed: {err:?}"))?;
        let root_inode = Self::iget_from_core_inode(&fs, root_inode)
            .inspect_err(|err| error!("KExt4 root inode VFS initialization failed: {err:?}"))?;
        let root = Dentry::new_dir_from_inode(root_inode, None, String::new());
        Ok(SuperBlock::new(fs, root))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, KExt4Core> {
        self.inner.lock()
    }

    pub(crate) const fn block_size(&self) -> u64 {
        self.block_size as u64
    }

    pub(crate) fn load_inode(&self, number: InodeNumber) -> VfsResult<Ext4Inode> {
        self.lock().inode(number).map_err(into_vfs_err)
    }

    fn metadata_from_core_inode(&self, inode: &Ext4Inode) -> VfsResult<Metadata> {
        let rdev = inode
            .device_id()
            .map_err(into_vfs_err)?
            .map_or(DeviceId::default(), ext4_device_id_to_vfs);
        Ok(Metadata {
            device: 0,
            inode: u64::from(inode.number().get()),
            nlink: u64::from(inode.links_count()),
            mode: Umode::from_bits(inode.mode()),
            uid: inode.uid(),
            gid: inode.gid(),
            size: inode.size(),
            block_size: self.block_size(),
            blocks: inode.blocks(),
            rdev,
            atime: ext4_timestamp_to_duration(inode.atime()),
            mtime: ext4_timestamp_to_duration(inode.mtime()),
            ctime: ext4_timestamp_to_duration(inode.ctime()),
        })
    }

    pub(crate) fn sync_vfs_directory(
        &self,
        vfs_inode: &VfsInode,
        core_inode: &Ext4Inode,
    ) -> VfsResult<()> {
        let metadata = self.metadata_from_core_inode(core_inode)?;
        vfs_inode.update_metadata_after_backing_change(&metadata)
    }

    pub(crate) fn sync_vfs_inode_attributes(
        &self,
        vfs_inode: &VfsInode,
        core_inode: &Ext4Inode,
    ) -> VfsResult<()> {
        let metadata = self.metadata_from_core_inode(core_inode)?;
        vfs_inode.update_attributes_after_backing_change(&metadata)
    }

    pub(crate) fn iget_from_core_inode(
        fs: &Arc<Self>,
        inode: Ext4Inode,
    ) -> VfsResult<Arc<VfsInode>> {
        let number = inode.number();
        if let Some(vfs_inode) = fs.inode_cache.lookup(u64::from(number.get())) {
            return Ok(vfs_inode);
        }
        let node_type = inode_kind_to_vfs(inode.kind());
        let metadata = fs.metadata_from_core_inode(&inode)?;
        let init = VfsInodeInit::new(u64::from(number.get()), metadata.size, metadata.mode)
            .with_owner_links_and_rdev(metadata.uid, metadata.gid, metadata.nlink, metadata.rdev)
            .with_stat_data(
                metadata.block_size,
                metadata.blocks,
                metadata.atime,
                metadata.mtime,
                metadata.ctime,
            );

        let vfs_inode = match node_type {
            NodeType::Directory => fs.inode_cache.get_or_insert_openable_dir_with_init(
                NodeFlags::empty(),
                init,
                || Inode::new(fs.clone(), number, node_type),
            ),
            NodeType::RegularFile | NodeType::Unknown => fs
                .inode_cache
                .get_or_insert_file_with_init(NodeFlags::empty(), init, || {
                    Inode::new(fs.clone(), number, node_type)
                }),
            NodeType::Symlink => {
                let cached_link = match fs.lock().symlink_storage(&inode) {
                    Ok(SymlinkStorage::Fast(target)) => {
                        core::str::from_utf8(target).ok().map(String::from)
                    }
                    _ => None,
                };
                let vfs_inode = fs.inode_cache.get_or_insert_symlink_with_init(
                    NodeFlags::empty(),
                    init,
                    || Inode::new(fs.clone(), number, node_type),
                );
                if let Some(link) = cached_link {
                    vfs_inode.set_cached_link(link);
                }
                vfs_inode
            }
            NodeType::CharacterDevice
            | NodeType::BlockDevice
            | NodeType::Fifo
            | NodeType::Socket => {
                fs.inode_cache
                    .get_or_insert_special_with_init(NodeFlags::empty(), init, || {
                        Inode::new(fs.clone(), number, node_type)
                    })
            }
        };
        Ok(vfs_inode)
    }

    pub(crate) fn make_dentry(
        fs: &Arc<Self>,
        parent: Option<Dentry>,
        name: String,
        inode: Ext4Inode,
    ) -> VfsResult<Dentry> {
        let inode = Self::iget_from_core_inode(fs, inode)?;
        if inode.is_dir() {
            Ok(Dentry::new_dir_from_inode(inode, parent, name))
        } else {
            Ok(Dentry::new_file_from_inode(inode, parent, name))
        }
    }

    pub(crate) fn sync_to_disk(&self) -> VfsResult<()> {
        self.lock().sync_filesystem().map_err(into_vfs_err)
    }

    pub(crate) fn reserve_delalloc_blocks(&self, blocks: u64) -> VfsResult<()> {
        if blocks == 0 {
            return Ok(());
        }

        let blocks_available = self.lock().blocks_available_for_reservation();
        let mut reserved = self.delalloc_reserved_blocks.lock();
        let available = blocks_available.saturating_sub(*reserved);
        if blocks > available {
            return Err(VfsError::StorageFull);
        }
        *reserved = reserved.saturating_add(blocks);
        Ok(())
    }

    pub(crate) fn release_delalloc_blocks(&self, blocks: u64) {
        if blocks == 0 {
            return;
        }
        let mut reserved = self.delalloc_reserved_blocks.lock();
        *reserved = reserved.saturating_sub(blocks);
    }
}

impl SuperBlockOperations for Ext4Filesystem {
    fn name(&self) -> &str {
        "ext4"
    }

    fn statfs(&self) -> VfsResult<StatFs> {
        let fs = self.lock();
        let stat = fs.statfs().map_err(into_vfs_err)?;
        let reserved = *self.delalloc_reserved_blocks.lock();
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size,
            blocks: stat.blocks,
            blocks_free: stat.blocks_free.saturating_sub(reserved),
            blocks_available: stat.blocks_available.saturating_sub(reserved),
            file_count: stat.files,
            free_file_count: stat.files_free,
            name_length: stat.max_name_len,
            fragment_size: stat.fragment_size,
            mount_flags: StatFsFlags::RELATIME,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.sync_to_disk()
    }

    fn max_file_size(&self) -> u64 {
        extent_max_file_size_bytes(self.block_size)
    }

    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        default_evict_inode(inode)?;
        let ext4_inode: Arc<Inode> = inode.downcast()?;
        ext4_inode.release_delalloc_for_eviction();
        if inode.metadata().nlink != 0 {
            return Ok(());
        }
        self.lock()
            .evict_unlinked_inode(ext4_inode.number(), current_ext4_timestamp())
            .map_err(into_vfs_err)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::extent_max_file_size_bytes;

    #[def_test]
    fn extent_file_size_limit_covers_all_logical_blocks() {
        assert_eq!(extent_max_file_size_bytes(1024), 1_u64 << 42);
        assert_eq!(extent_max_file_size_bytes(4096), 1_u64 << 44);
    }
}
