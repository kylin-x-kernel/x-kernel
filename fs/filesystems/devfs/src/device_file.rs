// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem node projection for character and block devices.

use alloc::{string::String, sync::Arc};

use inherit_methods_macro::inherit_methods;
use kvfs::{
    Dentry, DeviceFileOps, DeviceId, DirMapping, InodeOperations, Metadata, MetadataUpdate,
    NodeFlags, NodePermission, NodeType, SimpleDirLookup, SimpleFs, SimpleFsNode, VfsInode,
    VfsInodeInit, VfsResult, cdev_add,
};

/// A VFS node representing a character or block special file.
///
/// Character-device instances are installed in the device registry used by
/// special-file open. Block-device identity and registration are owned by
/// the block core; devfs only projects the already registered object into an inode.
pub struct DeviceFile {
    node: SimpleFsNode,
    character_operations: Option<Arc<dyn DeviceFileOps>>,
}

impl DeviceFile {
    /// Creates a character-device node and installs its operations table.
    ///
    /// Returns the registry error without publishing a node if the number is
    /// already registered. A zero number leaves the node unregistered.
    pub fn new_character(
        fs: Arc<SimpleFs>,
        device_id: DeviceId,
        ops: Arc<dyn DeviceFileOps>,
    ) -> VfsResult<Arc<Self>> {
        let node = SimpleFsNode::new(fs, NodeType::CharacterDevice, NodePermission::default());
        node.set_rdev(device_id);
        if device_id.0 != 0 {
            cdev_add(device_id, 1, ops.clone())?;
        }
        Ok(Arc::new(Self {
            node,
            character_operations: Some(ops),
        }))
    }

    /// Creates a block-device node that resolves through the block core.
    pub fn new_block(fs: Arc<SimpleFs>, device_id: DeviceId) -> Arc<Self> {
        let node = SimpleFsNode::new(fs, NodeType::BlockDevice, NodePermission::default());
        node.set_rdev(device_id);
        Arc::new(Self {
            node,
            character_operations: None,
        })
    }

    /// Returns `inode::i_flags` for this device inode.
    pub fn flags(&self) -> NodeFlags {
        self.character_operations
            .as_ref()
            .map_or(NodeFlags::empty(), |operations| operations.flags())
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
    ) -> VfsResult<MetadataUpdate>;
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
    device: VfsResult<Arc<DeviceFile>>,
) {
    let name = name.into();
    let device = match device {
        Ok(device) => device,
        Err(error) => {
            warn!("devtmpfs: cannot publish device {name}: {error:?}");
            return;
        }
    };
    root.add_child(name, move |lookup, name| {
        Ok(device_dentry(lookup, name, device.clone()))
    });
}
