// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! 9P-backed VFS inode, metadata, and namespace operations.

use alloc::{string::String, sync::Arc};

use kcred::Cred;
use ktime_types::SystemTime;
use kvfs::{
    Dentry, DeviceId, InodeDirOperations, InodeOperations, InodeSymlinkOperations, InodeUpdateTime,
    LockedDentry, Metadata, MetadataUpdate, MountIdmap, NodeFlags, NodePermission, NodeType,
    SuperBlock, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult, inode_init_owner,
};
use p9::{FileAttr, P9Fid, P9SetAttr, P9SetAttrTime, P9StatFs, Qid};

use super::util::dotl_decode_dev;

pub(crate) fn inode_init_from_attr(attr: &FileAttr, block_size: u64) -> VfsInodeInit {
    VfsInodeInit::new(
        qid_to_inode_number(attr.qid),
        attr.size,
        kvfs::Umode::from_bits(attr.mode as u16),
    )
    .with_owner_links_and_rdev(attr.uid, attr.gid, attr.nlink, dotl_decode_dev(attr.rdev))
    .with_generation(attr.generation as u32)
    .with_stat_data(
        block_size,
        attr.blocks,
        system_time_from_9p(attr.atime_sec, attr.atime_nsec),
        system_time_from_9p(attr.mtime_sec, attr.mtime_nsec),
        system_time_from_9p(attr.ctime_sec, attr.ctime_nsec),
    )
}

/// Maps a 9P qid to the user-visible inode number using Linux `QID2INO`.
pub(crate) const fn qid_to_inode_number(qid: Qid) -> u64 {
    qid.path().wrapping_add(2)
}

fn system_time_from_9p(seconds: u64, nanoseconds: u32) -> SystemTime {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    SystemTime::from_unix_parts(seconds, nanoseconds)
        .unwrap_or_else(|| SystemTime::from_unix_seconds(seconds))
}

fn system_time_to_9p(timestamp: SystemTime) -> VfsResult<P9SetAttrTime> {
    Ok(P9SetAttrTime::Explicit(
        u64::try_from(timestamp.unix_seconds()).map_err(|_| VfsError::InvalidInput)?,
        timestamp.subsec_nanos(),
    ))
}

/// Filesystem-private inode state, corresponding to Linux `v9fs_inode`.
///
/// KVFS currently has no dentry-private storage, so the unopened lookup fid is
/// owned here instead of by the dentry. Open file descriptions always receive
/// an independent walked and opened fid in their private data.
pub(crate) struct V9fsInode {
    pub(super) fid: Arc<P9Fid>,
}

impl V9fsInode {
    pub(crate) fn new(fid: Arc<P9Fid>) -> Arc<Self> {
        Arc::new(Self { fid })
    }

    pub(crate) fn into_vfs_inode(self: Arc<Self>, init: VfsInodeInit) -> Arc<VfsInode> {
        match init.node_type() {
            NodeType::Directory => {
                VfsInode::new_openable_dir_with_flags(self, NodeFlags::empty(), init)
            }
            NodeType::CharacterDevice
            | NodeType::BlockDevice
            | NodeType::Fifo
            | NodeType::Socket => VfsInode::new_special(self, NodeFlags::empty(), init),
            NodeType::RegularFile | NodeType::Symlink | NodeType::Unknown => {
                VfsInode::new_file_with_flags(self, NodeFlags::empty(), init)
            }
        }
    }

    pub(crate) fn validate_attr(fid: &P9Fid, attr: &FileAttr) -> VfsResult<()> {
        let fid_qid = fid.qid();
        let attr_qid = attr.qid;
        let node_type = Umode::from_bits(attr.mode as u16).node_type();
        if fid_qid.path() != attr_qid.path()
            || fid_qid.file_type() != attr_qid.file_type()
            || node_type == NodeType::Unknown
            || attr_qid.is_dir() != (node_type == NodeType::Directory)
            || attr_qid.is_symlink() != (node_type == NodeType::Symlink)
            || attr.atime_sec > i64::MAX as u64
            || attr.mtime_sec > i64::MAX as u64
            || attr.ctime_sec > i64::MAX as u64
        {
            return Err(VfsError::from(kerrno::LinuxError::EIO));
        }
        Ok(())
    }

    pub(crate) fn statfs(&self) -> VfsResult<P9StatFs> {
        self.fid.statfs()
    }

    fn getattr_checked(&self) -> VfsResult<FileAttr> {
        let attr = self.fid.getattr()?;
        Self::validate_attr(&self.fid, &attr)?;
        Ok(attr)
    }

    fn inode_from_fid(super_block: &Arc<SuperBlock>, fid: Arc<P9Fid>) -> VfsResult<Arc<VfsInode>> {
        let attr = fid.getattr()?;
        Self::validate_attr(&fid, &attr)?;
        Ok(Self::new(fid).into_vfs_inode(inode_init_from_attr(&attr, super_block.block_size())))
    }

    fn lookup_child(&self, super_block: &Arc<SuperBlock>, name: &str) -> VfsResult<Arc<VfsInode>> {
        let fid = self.fid.walk(&[name])?;
        Self::inode_from_fid(super_block, fid)
    }

    fn create_symlink_inode(
        &self,
        super_block: &Arc<SuperBlock>,
        name: &str,
        target: &str,
        gid: u32,
    ) -> VfsResult<Arc<VfsInode>> {
        self.fid.symlink(name, target, gid)?;
        self.lookup_child(super_block, name)
    }

    fn remove_child(
        &self,
        _dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        is_directory: bool,
    ) -> VfsResult<()> {
        let child = dentry.downcast::<Self>()?;
        match self.fid.unlink_at(dentry.name(), is_directory) {
            Ok(()) => {}
            Err(error) if error.canonicalize() == VfsError::OperationNotSupported => {
                let removal_fid = child.fid.walk(&[])?;
                removal_fid.remove()?;
            }
            Err(error) => return Err(error),
        }

        if is_directory {
            // Cacheless Linux v9fs only clears a removed directory's links;
            // all other link counts are refreshed from the server.
            dentry.inode_ref().clear_link_count();
        }
        Ok(())
    }
}

impl InodeOperations for V9fsInode {
    fn directory_operations(&self) -> Option<&dyn InodeDirOperations> {
        self.fid.qid().is_dir().then_some(self)
    }

    fn symlink_operations(&self) -> Option<&dyn InodeSymlinkOperations> {
        self.fid.qid().is_symlink().then_some(self)
    }

    fn getattr(
        &self,
        _idmap: &MountIdmap,
        _path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        let attr = self.getattr_checked()?;
        Ok(Metadata {
            inode: qid_to_inode_number(attr.qid),
            device: 0,
            nlink: attr.nlink,
            mode: kvfs::Umode::from_bits(attr.mode as u16),
            uid: attr.uid,
            gid: attr.gid,
            size: attr.size,
            block_size: attr.blksize,
            blocks: attr.blocks,
            rdev: dotl_decode_dev(attr.rdev),
            atime: system_time_from_9p(attr.atime_sec, attr.atime_nsec),
            mtime: system_time_from_9p(attr.mtime_sec, attr.mtime_nsec),
            ctime: system_time_from_9p(attr.ctime_sec, attr.ctime_nsec),
        })
    }

    fn setattr(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        let (uid, gid) = update.owner.unzip();
        self.fid.setattr(P9SetAttr {
            mode: update.mode.map(|mode| u32::from(mode.bits())),
            uid,
            gid,
            size: update.size,
            atime: update.atime.map(system_time_to_9p).transpose()?,
            mtime: update.mtime.map(system_time_to_9p).transpose()?,
            should_update_ctime: update.ctime.is_some(),
        })?;
        // TSETATTR can request a server ctime update but cannot assign its value;
        // RSETATTR carries no timestamp that KVFS could publish as confirmed.
        Ok(MetadataUpdate {
            ctime: None,
            ..update
        })
    }

    fn update_time(
        &self,
        _idmap: &MountIdmap,
        _dentry: &Dentry,
        timestamp: SystemTime,
        update: InodeUpdateTime,
    ) -> VfsResult<MetadataUpdate> {
        // Linux v9fs uses generic_update_time for automatic inode timestamps.
        // Explicit remote attribute changes belong to setattr, not this hook.
        let update = match update {
            InodeUpdateTime::Access => MetadataUpdate {
                atime: Some(timestamp),
                ..Default::default()
            },
            InodeUpdateTime::ChangeAndModification => MetadataUpdate {
                mtime: Some(timestamp),
                ctime: Some(timestamp),
                ..Default::default()
            },
        };
        Ok(update)
    }
}

impl InodeSymlinkOperations for V9fsInode {
    fn get_link(
        &self,
        _dentry: Option<&Dentry>,
        _inode: &VfsInode,
        _done: &mut kvfs::DelayedCall,
    ) -> VfsResult<String> {
        self.fid.read_link()
    }
}

impl InodeDirOperations for V9fsInode {
    fn lookup(
        &self,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        _flags: kvfs::InodeLookupFlags,
    ) -> VfsResult<Option<Dentry>> {
        let inode = match self.lookup_child(&dir.super_block()?, dentry.name()) {
            Ok(inode) => inode,
            Err(error) if error.canonicalize() == VfsError::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        dentry.instantiate_or_alias(inode)
    }

    fn create(
        &self,
        _idmap: &MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        _exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        if mode.node_type() != NodeType::RegularFile {
            return Err(VfsError::InvalidInput);
        }

        self.fid
            .mknod(dentry.name(), u32::from(mode.bits()), 0, 0, gid)?;
        let inode = self.lookup_child(&dir.super_block()?, dentry.name())?;
        dentry.instantiate(inode)
    }

    fn mkdir(
        &self,
        _idmap: &MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        if mode.node_type() != NodeType::Directory {
            return Err(VfsError::InvalidInput);
        }
        self.fid.mkdir(dentry.name(), u32::from(mode.bits()), gid)?;
        let inode = self.lookup_child(&dir.super_block()?, dentry.name())?;
        dentry.instantiate(inode)?;
        dir.increment_link_count();
        Ok(())
    }

    fn mknod(
        &self,
        _idmap: &MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        mode: Umode,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (mode, _, gid) = inode_init_owner(dir, mode, cred);
        if matches!(
            mode.node_type(),
            NodeType::Directory | NodeType::RegularFile | NodeType::Symlink | NodeType::Unknown
        ) {
            return Err(VfsError::InvalidInput);
        }
        self.fid.mknod(
            dentry.name(),
            u32::from(mode.bits()),
            device.major(),
            device.minor(),
            gid,
        )?;
        let inode = self.lookup_child(&dir.super_block()?, dentry.name())?;
        dentry.instantiate(inode)
    }

    fn symlink(
        &self,
        _idmap: &MountIdmap,
        dir: &VfsInode,
        dentry: &LockedDentry<'_>,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
        let (_, _, gid) = inode_init_owner(
            dir,
            Umode::new(NodeType::Symlink, NodePermission::from_bits_truncate(0o777)),
            cred,
        );
        let inode = self.create_symlink_inode(&dir.super_block()?, dentry.name(), target, gid)?;
        dentry.instantiate(inode)
    }

    fn link(&self, node: &Dentry, _dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        let target = node.downcast::<Self>()?;
        self.fid.link(&target.fid, dentry.name())?;
        dentry.instantiate(node.inode_ref())?;
        Ok(())
    }

    fn unlink(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.remove_child(dir, dentry, false)
    }

    fn rmdir(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        self.remove_child(dir, dentry, true)
    }

    fn rename(
        &self,
        _idmap: &MountIdmap,
        _old_dir: &VfsInode,
        old_dentry: &LockedDentry<'_>,
        new_dir: &VfsInode,
        new_dentry: &LockedDentry<'_>,
        flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        if !flags.is_empty() {
            return Err(VfsError::InvalidInput);
        }
        let source = old_dentry.downcast::<Self>()?;
        let destination_dir = new_dir.private::<Self>()?;
        match self
            .fid
            .rename_at(old_dentry.name(), &destination_dir.fid, new_dentry.name())
        {
            Ok(()) => {}
            Err(error) if error.canonicalize() == VfsError::OperationNotSupported => {
                source.fid.rename(&destination_dir.fid, new_dentry.name())?;
            }
            Err(error) => return Err(error),
        }

        let replaced = if new_dentry.is_positive() {
            Some(new_dentry.inode_ref())
        } else {
            None
        };
        if let Some(replaced) = &replaced
            && replaced.is_dir()
        {
            replaced.clear_link_count();
        }
        if source.fid.qid().is_dir() && replaced.is_none() {
            new_dir.increment_link_count();
        }
        Ok(())
    }
}

#[unittest::mod_test]
mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use p9::{P9Client, P9Transport};
    use unittest::{assert_eq, def_test};

    use super::*;

    #[derive(Clone, Default)]
    struct MetadataTransport {
        requests: Arc<AtomicUsize>,
        setattr_valid: Arc<AtomicU32>,
        should_fail: Arc<AtomicBool>,
    }

    impl P9Transport for MetadataTransport {
        fn request(&mut self, request: &[u8], response: &mut [u8]) -> VfsResult<usize> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            response[5..7].copy_from_slice(&request[5..7]);
            let size: u32 = match request[4] {
                100 => {
                    response[4] = 101; // RVERSION
                    response[7..11].copy_from_slice(&8192u32.to_le_bytes());
                    response[11..13].copy_from_slice(&8u16.to_le_bytes());
                    response[13..21].copy_from_slice(b"9P2000.L");
                    21
                }
                104 => {
                    response[4] = 105; // RATTACH, regular-file qid
                    response[7..20].fill(0);
                    response[12..20].copy_from_slice(&1u64.to_le_bytes());
                    20
                }
                26 => {
                    self.setattr_valid.store(
                        u32::from_le_bytes(request[11..15].try_into().unwrap()),
                        Ordering::Relaxed,
                    );
                    if self.should_fail.load(Ordering::Relaxed) {
                        return Err(VfsError::Io);
                    }
                    response[4] = 27; // RSETATTR
                    7
                }
                120 => {
                    response[4] = 121; // RCLUNK
                    7
                }
                _ => return Err(VfsError::InvalidInput),
            };
            response[..4].copy_from_slice(&size.to_le_bytes());
            Ok(size as usize)
        }
    }

    fn metadata_fixture() -> (MetadataTransport, Arc<V9fsInode>, Dentry) {
        let transport = MetadataTransport::default();
        let client = P9Client::new(Box::new(transport.clone()));
        let node = V9fsInode::new(client.negotiate().unwrap());
        let inode = node.clone().into_vfs_inode(VfsInodeInit::new(
            3,
            16,
            Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            ),
        ));
        let dentry = Dentry::new_file_from_inode(inode, None, String::from("timestamp"));
        (transport, node, dentry)
    }

    #[def_test]
    fn automatic_timestamps_do_not_issue_remote_setattr() {
        let (transport, node, dentry) = metadata_fixture();
        let requests = transport.requests.load(Ordering::Relaxed);
        // Automatic timestamps must remain local even if remote setattr fails.
        transport.should_fail.store(true, Ordering::Relaxed);
        let timestamp = SystemTime::from_unix_seconds(123);
        let applied = node
            .update_time(&MountIdmap, &dentry, timestamp, InodeUpdateTime::Access)
            .unwrap();
        assert_eq!(applied.atime, Some(timestamp));
        assert_eq!(applied.mtime, None);
        assert_eq!(applied.ctime, None);
        let applied = node
            .update_time(
                &MountIdmap,
                &dentry,
                timestamp,
                InodeUpdateTime::ChangeAndModification,
            )
            .unwrap();
        assert_eq!(applied.atime, None);
        assert_eq!(applied.mtime, Some(timestamp));
        assert_eq!(applied.ctime, Some(timestamp));
        assert_eq!(transport.requests.load(Ordering::Relaxed), requests);
    }

    #[def_test]
    fn explicit_setattr_does_not_publish_unconfirmed_ctime() {
        let (transport, _node, dentry) = metadata_fixture();
        let initial_ctime = dentry.inode_ref().metadata().ctime;
        let timestamp = SystemTime::from_unix_seconds(456);
        dentry
            .update_metadata(MetadataUpdate {
                size: Some(0),
                atime: Some(timestamp),
                mtime: Some(timestamp),
                ctime: Some(timestamp),
                ..Default::default()
            })
            .unwrap();
        // SIZE, ATIME, MTIME, CTIME, ATIME_SET, MTIME_SET.
        assert_eq!(transport.setattr_valid.load(Ordering::Relaxed), 0x1f8);
        let metadata = dentry.inode_ref().metadata();
        assert_eq!(metadata.size, 0);
        assert_eq!(metadata.atime, timestamp);
        assert_eq!(metadata.mtime, timestamp);
        assert_eq!(metadata.ctime, initial_ctime);

        transport.should_fail.store(true, Ordering::Relaxed);
        assert_eq!(
            dentry.update_metadata(MetadataUpdate {
                size: Some(8),
                atime: Some(SystemTime::from_unix_seconds(789)),
                ..Default::default()
            }),
            Err(VfsError::Io)
        );
        let metadata = dentry.inode_ref().metadata();
        assert_eq!(metadata.size, 0);
        assert_eq!(metadata.atime, timestamp);
        assert_eq!(metadata.ctime, initial_ctime);
    }
}
