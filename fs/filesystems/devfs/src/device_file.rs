// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file node — a VFS adapter that wraps [`DeviceFileOps`] into a
//! filesystem-accessible special file (char/block device).

use alloc::sync::Arc;
use core::any::Any;

use inherit_methods_macro::inherit_methods;
use kpoll::{IoEvents, Pollable};
use kvfs::{
    DeviceFileOps, DeviceId, FileNodeOps, Metadata, MetadataUpdate, MmapMapper, NodeFlags, NodeOps,
    NodePermission, NodeType, VfsError, VfsResult,
};
use kvfs_simple::{SimpleFs, SimpleFsNode};

/// A VFS node representing a device special file (char/block).
///
/// Wraps a [`DeviceFileOps`] implementor and adapts it to the standard VFS
/// interfaces ([`NodeOps`], [`FileNodeOps`], [`Pollable`]).
pub struct DeviceFile {
    node: SimpleFsNode,
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
        node.set_device_id(device_id);
        Arc::new(Self { node, ops })
    }

    /// Returns the inner device file operations.
    pub fn inner(&self) -> &Arc<dyn DeviceFileOps> {
        &self.ops
    }

    /// Updates the device ID.
    pub fn set_device_id(&self, device_id: DeviceId) {
        self.node.set_device_id(device_id);
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for DeviceFile {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata>;

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(0)
    }

    fn flags(&self) -> NodeFlags {
        self.ops.flags()
    }
}

impl FileNodeOps for DeviceFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.ops.read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.ops.write_at(buf, offset)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::NotATty)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        if self.write_at(b"", 0).is_ok() {
            Ok(())
        } else {
            Err(VfsError::BadFileDescriptor)
        }
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.ops.ioctl(cmd, arg)
    }

    fn mmap(&self, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.ops.mmap(mapper)
    }
}

impl Pollable for DeviceFile {
    fn poll(&self) -> IoEvents {
        if let Some(pollable) = self.ops.as_pollable() {
            pollable.poll()
        } else {
            IoEvents::IN | IoEvents::OUT
        }
    }

    fn register(&self, context: &mut core::task::Context<'_>, events: IoEvents) {
        if let Some(pollable) = self.ops.as_pollable() {
            pollable.register(context, events);
        }
    }
}
