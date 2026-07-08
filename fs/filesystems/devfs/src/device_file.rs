// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file node — a VFS adapter that wraps [`DeviceFileOps`] into a
//! filesystem-accessible special file (char/block device).

use alloc::{string::String, sync::Arc};

use inherit_methods_macro::inherit_methods;
use kpoll::{IoEvents, Pollable};
use kvfs::{
    Dentry, DeviceFileOps, DeviceId, DirMapping, InodeOperations, Metadata, MetadataUpdate,
    MmapMapper, NodeFlags, NodePermission, NodeType, SimpleDirLookup, SimpleFs, SimpleFsNode,
    VfsFile, VfsInode, VfsInodeInit, VfsResult, bdev_add, cdev_add,
};

/// A VFS node representing a device special file (char/block).
///
/// Wraps a [`DeviceFileOps`] implementor and installs it in the device
/// registry used by special-file open.
pub struct DeviceFile {
    node: SimpleFsNode,
    node_type: NodeType,
    ops: Arc<dyn DeviceFileOps>,
}

impl DeviceFile {
    /// Creates a new device file node.
    pub fn new(
        fs: Arc<SimpleFs>,
        node_type: NodeType,
        device_id: DeviceId,
        ops: Arc<dyn DeviceFileOps>,
    ) -> Arc<Self> {
        let node = SimpleFsNode::new(fs, node_type, NodePermission::default());
        node.set_rdev(device_id);
        if device_id.0 != 0 {
            match node_type {
                NodeType::CharacterDevice => cdev_add(device_id, ops.clone()),
                NodeType::BlockDevice => bdev_add(device_id, ops.clone()),
                _ => {}
            }
        }
        Arc::new(Self {
            node,
            node_type,
            ops,
        })
    }

    /// Returns the inner device file operations.
    pub fn inner(&self) -> &Arc<dyn DeviceFileOps> {
        &self.ops
    }

    /// Updates the device ID.
    pub fn set_device_id(&self, device_id: DeviceId) {
        self.node.set_rdev(device_id);
        if device_id.0 != 0 {
            match self.node_type {
                NodeType::CharacterDevice => cdev_add(device_id, self.ops.clone()),
                NodeType::BlockDevice => bdev_add(device_id, self.ops.clone()),
                _ => {}
            }
        }
    }

    /// Returns `inode::i_flags` for this device inode.
    pub fn flags(&self) -> NodeFlags {
        self.ops.flags()
    }

    /// Returns the inode fields used when materializing this device node.
    pub fn inode_init(&self) -> VfsInodeInit {
        self.node.inode_init()
    }
}

#[inherit_methods(from = "self.node")]
impl InodeOperations for DeviceFile {
    fn getattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _path: Option<&kvfs::Path>,
        _request_mask: kvfs::GetattrRequestMask,
        _query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata>;

    fn setattr(
        &self,
        _idmap: &kvfs::MountIdmap,
        _dentry: &kvfs::Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<()>;
}

impl kvfs::FileOperations for DeviceFile {
    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.ops.read(file, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.ops.write(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.ops.ioctl(file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.ops.mmap(file, mapper)
    }
}

impl Pollable for DeviceFile {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut core::task::Context<'_>, _events: IoEvents) {}
}

pub(crate) fn device_dentry(
    lookup: SimpleDirLookup<'_>,
    name: &str,
    device: Arc<DeviceFile>,
) -> Dentry {
    let i_flags = NodeFlags::NON_CACHEABLE | device.flags();
    let init = device.inode_init();
    let inode = VfsInode::new_special(device, i_flags, init);
    lookup.file_from_inode(name, inode)
}

pub(crate) fn add_device_entry(
    root: &mut DirMapping,
    name: impl Into<String>,
    device: Arc<DeviceFile>,
) {
    root.add_child(name, move |lookup, name| {
        Ok(device_dentry(lookup, name, device.clone()))
    });
}
