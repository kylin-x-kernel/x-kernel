// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KExt4 superblock operations.

use alloc::{string::String, sync::Arc};

use kext4::{
    Ext4Error, Ext4Filesystem as KExt4Core, Ext4Inode, Ext4StatFsMode, Ext4SyncIntent, InodeNumber,
};
use ksync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use kvfs::{
    Dentry, DeviceId, Metadata, NodeFlags, NodeType, StatFs, SuperBlock, SuperBlockOperations,
    Umode, VfsInode, VfsInodeInit, VfsResult, default_evict_inode,
};

use super::{
    inode::Inode,
    util::{
        current_ext4_timestamp, ext4_device_id_to_vfs, ext4_timestamp_to_system_time,
        inode_kind_to_vfs, into_vfs_err,
    },
};

const EXT4_ROOT_INO: u32 = 2;

/// Per-batch maximum for phased eviction block release.
/// 256 blocks × 4 KiB = 1 MiB of block allocation work per transaction.
const EVICTION_BATCH_BLOCKS: u32 = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Ext4MountOptions {
    statfs_mode: Ext4StatFsMode,
}

impl Ext4MountOptions {
    pub(crate) fn parse(data: Option<&[u8]>) -> kvfs::VfsResult<Self> {
        let mut options = Self::default();
        let data = data.unwrap_or_default();
        let text = &data[..data
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(data.len())];
        for option in text.split(|byte| *byte == b',') {
            match option {
                b"" => {}
                b"minixdf" => options.statfs_mode = Ext4StatFsMode::Minix,
                b"bsddf" => options.statfs_mode = Ext4StatFsMode::Bsd,
                _ => return Err(kvfs::VfsError::InvalidInput),
            }
        }
        Ok(options)
    }
}

fn node_flags_from_core_inode(inode: &Ext4Inode) -> NodeFlags {
    let (immutable, append_only) = inode.inode_attr_flags();
    let mut flags = NodeFlags::empty();
    flags.set(NodeFlags::IMMUTABLE, immutable);
    flags.set(NodeFlags::APPEND_ONLY, append_only);
    flags
}

/// Ext4 filesystem implementation backed by the checked KExt4 core.
pub struct Ext4Filesystem {
    inner: RwLock<KExt4Core>,
    statfs_mode: Ext4StatFsMode,
    /// Mount-invariant block size, cached at mount time.
    ///
    /// This mirrors Linux `s_blocksize`: it is fixed once and never changes,
    /// so it is cached without taking the filesystem lock on every access.
    block_size: u32,
    extent_max_file_size: u64,
    legacy_max_file_size: u64,
}

impl Ext4Filesystem {
    /// Fills a newly reserved KExt4 superblock from a validated block device.
    ///
    /// # Errors
    ///
    /// Returns an error when ext4 validation, recovery, geometry discovery,
    /// or root inode initialization fails.
    pub(crate) fn fill_super(
        super_block: &Arc<SuperBlock>,
        mount_options: Ext4MountOptions,
    ) -> VfsResult<()> {
        let dev = super_block
            .block_device()
            .expect("get_tree_bdev must set s_bdev before fill_super")
            .clone();
        let device: Arc<dyn block::BlockDeviceOperations> = dev;
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
                        error!("KExt4 mount after filesystem recovery failed: {err:?}");
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
        let extent_max_file_size = core.extent_max_file_size().map_err(into_vfs_err)?;
        let legacy_max_file_size = core.legacy_max_file_size().map_err(into_vfs_err)?;
        let fs = Arc::new(Self {
            inner: RwLock::new(core),
            statfs_mode: mount_options.statfs_mode,
            block_size,
            extent_max_file_size,
            legacy_max_file_size,
        });
        super_block.initialize(fs.clone(), |super_block| {
            let root_inode = Self::iget(super_block, &fs, InodeNumber::new(EXT4_ROOT_INO))
                .inspect_err(|err| error!("KExt4 root inode VFS initialization failed: {err:?}"))?;
            Ok(Dentry::new_dir_from_inode(root_inode, None, String::new()))
        })
    }

    pub(crate) fn lock(&self) -> RwLockWriteGuard<'_, KExt4Core> {
        self.inner.write()
    }

    /// Acquires the shared (read) lock for read-only operations such as
    /// `statfs`, inode loads, directory lookups, and extent/hole queries.
    /// Multiple readers run concurrently, mirroring Linux ext4's read-side
    /// concurrency for statfs and the per-inode extent tree.
    pub(crate) fn read_lock(&self) -> RwLockReadGuard<'_, KExt4Core> {
        self.inner.read()
    }

    pub(crate) const fn max_file_size_for_format(&self, has_extents: bool) -> u64 {
        if has_extents {
            self.extent_max_file_size
        } else {
            self.legacy_max_file_size
        }
    }

    pub(crate) const fn block_size(&self) -> u64 {
        self.block_size as u64
    }

    pub(crate) fn metadata_from_core_inode(&self, inode: &Ext4Inode) -> Metadata {
        let stat = inode.stat();
        let rdev = stat.rdev.map_or(DeviceId::default(), ext4_device_id_to_vfs);
        Metadata {
            device: 0,
            inode: u64::from(inode.number().get()),
            nlink: u64::from(stat.links_count),
            mode: Umode::from_bits(stat.mode),
            uid: stat.uid,
            gid: stat.gid,
            size: stat.size,
            block_size: self.block_size(),
            blocks: stat.blocks,
            rdev,
            atime: ext4_timestamp_to_system_time(stat.atime),
            mtime: ext4_timestamp_to_system_time(stat.mtime),
            ctime: ext4_timestamp_to_system_time(stat.ctime),
        }
    }

    pub(crate) fn iget(
        super_block: &Arc<SuperBlock>,
        fs: &Arc<Self>,
        number: InodeNumber,
    ) -> VfsResult<Arc<VfsInode>> {
        super_block.get_or_try_init_inode(u64::from(number.get()), || {
            let inode = fs
                .read_lock()
                .load_inode_private(number)
                .map_err(into_vfs_err)?;
            Self::new_vfs_inode(fs, inode)
        })
    }

    pub(crate) fn iget_from_core_inode(
        super_block: &Arc<SuperBlock>,
        fs: &Arc<Self>,
        inode: Ext4Inode,
    ) -> VfsResult<Arc<VfsInode>> {
        let number = inode.number();
        super_block
            .get_or_try_init_inode(u64::from(number.get()), || Self::new_vfs_inode(fs, inode))
    }

    fn new_vfs_inode(fs: &Arc<Self>, inode: Ext4Inode) -> VfsResult<Arc<VfsInode>> {
        let number = inode.number();
        let node_type = inode_kind_to_vfs(inode.kind());
        let node_flags = node_flags_from_core_inode(&inode);
        let metadata = fs.metadata_from_core_inode(&inode);
        let init = VfsInodeInit::new(u64::from(number.get()), metadata.size, metadata.mode)
            .with_owner_links_and_rdev(metadata.uid, metadata.gid, metadata.nlink, metadata.rdev)
            .with_generation(inode.generation())
            .with_stat_data(
                metadata.block_size,
                metadata.blocks,
                metadata.atime,
                metadata.mtime,
                metadata.ctime,
            );

        let cached_link = if node_type == NodeType::Symlink {
            match fs.read_lock().fast_symlink_target(&inode) {
                Ok(Some(target)) => core::str::from_utf8(&target).ok().map(String::from),
                _ => None,
            }
        } else {
            None
        };
        let vfs_inode = VfsInode::new_with_inode_attribute_operations(
            Inode::new(fs.clone(), inode, node_type),
            node_flags,
            init,
        );
        if let Some(link) = cached_link {
            vfs_inode.set_cached_link(link);
        }
        Ok(vfs_inode)
    }

    pub(crate) fn sync_to_disk(&self) -> VfsResult<()> {
        self.lock().sync_filesystem().map_err(into_vfs_err)
    }

    pub(crate) fn sync_inode_to_disk(
        &self,
        inode: &Ext4Inode,
        intent: Ext4SyncIntent,
    ) -> VfsResult<()> {
        let mut fs = self.lock();
        fs.sync_inode(inode, intent).map_err(into_vfs_err)
    }
}

impl SuperBlockOperations for Ext4Filesystem {
    fn statfs(&self) -> VfsResult<StatFs> {
        let fs = self.read_lock();
        let stat = fs
            .statfs_with_mode(self.statfs_mode)
            .map_err(into_vfs_err)?;
        Ok(StatFs {
            fs_type: 0xef53,
            block_size: stat.block_size,
            blocks: stat.blocks,
            blocks_free: stat.blocks_free,
            blocks_available: stat.blocks_available,
            file_count: stat.files,
            free_file_count: stat.files_free,
            name_length: stat.max_name_len,
            fragment_size: stat.fragment_size,
        })
    }

    fn sync_fs(&self) -> VfsResult<()> {
        self.sync_to_disk()
    }

    fn max_file_size(&self) -> u64 {
        self.extent_max_file_size
    }

    fn evict_inode(&self, inode: &VfsInode) -> VfsResult<()> {
        default_evict_inode(inode)?;
        let ext4_inode: Arc<Inode> = inode.downcast()?;
        self.lock()
            .release_all_delalloc(ext4_inode.core_inode())
            .map_err(into_vfs_err)?;
        if inode.link_count() != 0 {
            return Ok(());
        }

        let timestamp = current_ext4_timestamp();

        // Phase A: orphan + xattr (extent tree is NOT truncated — it
        // serves as the persistent record of blocks to free)
        self.lock()
            .eviction_prepare(ext4_inode.core_inode())
            .map_err(into_vfs_err)?;

        // Phase B: atomically truncate extent tree in batches, each
        // releasing both extent mappings and underlying physical blocks
        // in a single transaction (lock released between batches)
        loop {
            let (_, done) = {
                let mut core = self.lock();
                core.eviction_release_batch(ext4_inode.core_inode(), EVICTION_BATCH_BLOCKS)
                    .map_err(into_vfs_err)?
            };
            if done {
                break;
            }
        }

        // Phase C: update metadata, remove orphan, release inode slot
        self.lock()
            .eviction_finish(ext4_inode.core_inode(), timestamp)
            .map_err(into_vfs_err)
    }
}

#[cfg(unittest)]
mod tests {
    use kext4::Ext4StatFsMode;
    use unittest::{assert_eq, def_test};

    use super::Ext4MountOptions;

    #[def_test]
    fn statfs_mount_options_default_to_bsddf() {
        assert_eq!(
            Ext4MountOptions::parse(None).unwrap().statfs_mode,
            Ext4StatFsMode::Bsd
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"\0")).unwrap().statfs_mode,
            Ext4StatFsMode::Bsd
        );
    }

    #[def_test]
    fn last_statfs_mount_option_wins() {
        assert_eq!(
            Ext4MountOptions::parse(Some(b"bsddf,minixdf\0"))
                .unwrap()
                .statfs_mode,
            Ext4StatFsMode::Minix
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"minixdf,,bsddf\0"))
                .unwrap()
                .statfs_mode,
            Ext4StatFsMode::Bsd
        );
    }

    #[def_test]
    fn unsupported_mount_options_are_rejected() {
        assert_eq!(
            Ext4MountOptions::parse(Some(b"garbage\0")).unwrap_err(),
            kvfs::VfsError::InvalidInput
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"discard\0")).unwrap_err(),
            kvfs::VfsError::InvalidInput
        );
        assert_eq!(
            Ext4MountOptions::parse(Some(b"nodelalloc\0")).unwrap_err(),
            kvfs::VfsError::InvalidInput
        );
    }

    #[def_test]
    fn bytes_after_mount_data_terminator_are_ignored() {
        assert_eq!(
            Ext4MountOptions::parse(Some(b"minixdf\0\xff"))
                .unwrap()
                .statfs_mode,
            Ext4StatFsMode::Minix
        );
    }
}
