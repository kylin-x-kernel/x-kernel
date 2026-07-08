// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping callback.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::task::Context;

use hashbrown::HashMap;
use kerrno::LinuxError;
use kpoll::IoEvents;
use memaddr::PhysAddrRange;

use crate::{
    DeviceId, FileOperations, Mutex, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode,
    VfsResult,
};

struct ChrdevEntry {
    ops: Arc<dyn DeviceFileOps>,
    inodes: Vec<Weak<VfsInode>>,
}

impl ChrdevEntry {
    fn new(ops: Arc<dyn DeviceFileOps>) -> Self {
        Self {
            ops,
            inodes: Vec::new(),
        }
    }

    fn clear_inodes(&self) {
        for inode in &self.inodes {
            if let Some(inode) = inode.upgrade() {
                inode.clear_character_device(&self.ops);
            }
        }
    }
}

struct ChrdevRegistry(Mutex<Option<HashMap<u64, ChrdevEntry>>>);

impl ChrdevRegistry {
    fn with<R>(&self, f: impl FnOnce(&mut HashMap<u64, ChrdevEntry>) -> R) -> R {
        let mut registry = self.0.lock();
        f(registry.get_or_insert_with(HashMap::new))
    }

    fn add(&self, device: DeviceId, ops: Arc<dyn DeviceFileOps>) {
        if let Some(old) = self.with(|registry| registry.insert(device.0, ChrdevEntry::new(ops))) {
            old.clear_inodes();
        }
    }

    fn del(&self, device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
        self.with(|registry| registry.remove(&device.0))
            .map(|entry| {
                entry.clear_inodes();
                entry.ops
            })
    }

    fn get(&self, inode: &Arc<VfsInode>) -> Option<Arc<dyn DeviceFileOps>> {
        if let Some(ops) = inode.character_device() {
            return Some(ops);
        }

        self.with(|registry| {
            let entry = registry.get_mut(&inode.rdev().0)?;
            let ops = entry.ops.clone();
            inode.set_character_device(ops.clone());
            entry.inodes.push(Arc::downgrade(inode));
            Some(ops)
        })
    }

    fn open(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let f_inode = file.inode()?;
        debug_assert!(core::ptr::eq(inode, f_inode.as_ref()));
        let ops = self
            .get(f_inode)
            .ok_or_else(|| VfsError::from(LinuxError::ENXIO))?;
        let fops = Arc::new(ChrdevFileOperations::new(ops));
        file.replace_fops(fops.clone());
        fops.open(inode, file)
    }
}

struct BdevRegistry(Mutex<Option<HashMap<u64, Arc<dyn DeviceFileOps>>>>);

impl BdevRegistry {
    fn with<R>(&self, f: impl FnOnce(&mut HashMap<u64, Arc<dyn DeviceFileOps>>) -> R) -> R {
        let mut registry = self.0.lock();
        f(registry.get_or_insert_with(HashMap::new))
    }

    fn add(&self, device: DeviceId, ops: Arc<dyn DeviceFileOps>) {
        self.with(|registry| {
            registry.insert(device.0, ops);
        });
    }

    fn del(&self, device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
        self.with(|registry| registry.remove(&device.0))
    }

    fn get(&self, device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
        self.with(|registry| registry.get(&device.0).cloned())
    }

    fn get_file_device(&self, file: &VfsFile) -> VfsResult<Arc<dyn DeviceFileOps>> {
        self.get(file.inode().rdev())
            .ok_or_else(|| VfsError::from(LinuxError::ENXIO))
    }

    fn open(&self, inode: &VfsInode) -> VfsResult<()> {
        self.get(inode.rdev())
            .map(|_| ())
            .ok_or_else(|| VfsError::from(LinuxError::ENXIO))
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.get_file_device(file)?.read(file, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.get_file_device(file)?.write(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.get_file_device(file)?.ioctl(file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.get_file_device(file)?.mmap(file, mapper)
    }
}

static CHRDEV_REGISTRY: ChrdevRegistry = ChrdevRegistry(Mutex::new(None));
static BDEV_REGISTRY: BdevRegistry = BdevRegistry(Mutex::new(None));

/// Register a character-device operation table for a device number.
pub fn cdev_add(device: DeviceId, ops: Arc<dyn DeviceFileOps>) {
    CHRDEV_REGISTRY.add(device, ops);
}

/// Remove a character-device operation table for a device number.
pub fn cdev_del(device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
    CHRDEV_REGISTRY.del(device)
}

/// Add a block device to the VFS block-device map.
pub fn bdev_add(device: DeviceId, ops: Arc<dyn DeviceFileOps>) {
    BDEV_REGISTRY.add(device, ops);
}

/// Remove a block device from the VFS block-device map.
pub fn bdev_del(device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
    BDEV_REGISTRY.del(device)
}

struct ChrdevFileOperations {
    ops: Arc<dyn DeviceFileOps>,
}

impl ChrdevFileOperations {
    fn new(ops: Arc<dyn DeviceFileOps>) -> Self {
        Self { ops }
    }
}

impl FileOperations for ChrdevFileOperations {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        self.ops.open(inode, file)
    }

    fn supports_read(&self) -> bool {
        self.ops.supports_read()
    }

    fn supports_write(&self) -> bool {
        self.ops.supports_write()
    }

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

    fn poll(&self, file: &VfsFile) -> IoEvents {
        self.ops.poll(file)
    }

    fn register_poll(&self, file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        self.ops.register_poll(file, context, events);
    }
}

struct DefaultChrdevFileOperations;
struct DefaultBlkdevFileOperations;

impl FileOperations for DefaultChrdevFileOperations {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        CHRDEV_REGISTRY.open(inode, file)
    }
}

impl FileOperations for DefaultBlkdevFileOperations {
    fn open(self: Arc<Self>, inode: &VfsInode, _file: &mut VfsFileBuilder) -> VfsResult<()> {
        BDEV_REGISTRY.open(inode)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        BDEV_REGISTRY.read(file, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        BDEV_REGISTRY.write(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        BDEV_REGISTRY.ioctl(file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        BDEV_REGISTRY.mmap(file, mapper)
    }
}

pub(crate) fn character_device_file_operations() -> Arc<dyn FileOperations> {
    Arc::new(DefaultChrdevFileOperations)
}

pub(crate) fn block_device_file_operations() -> Arc<dyn FileOperations> {
    Arc::new(DefaultBlkdevFileOperations)
}

/// Callback trait for establishing memory mappings.
///
/// Passed to file mmap operations so that devices and files can request mapping
/// establishment without depending on the memory subsystem.
/// Implemented by the mmap syscall layer (posix-mm).
pub trait MmapMapper {
    /// Map a physical address range (device memory, framebuffer, etc.)
    fn map_physical(&mut self, range: PhysAddrRange) -> VfsResult<()>;

    /// Request a file-backed mapping (regular file or cached file).
    fn map_file_backed(&mut self) -> VfsResult<()>;

    /// Request a shared anonymous mapping.
    ///
    /// This is used by special nodes such as `/dev/zero`, whose mmap
    /// semantics are object-less shared anonymous memory rather than ordinary
    /// file-backed page cache.
    fn map_anonymous_shared(&mut self) -> VfsResult<()>;

    /// Returns the mmap offset supplied by userspace.
    ///
    /// Devices that multiplex multiple buffers behind a single fd (e.g. DRM
    /// dumb buffers) use this offset as a lookup key rather than a byte offset
    /// into a single contiguous region.
    fn offset(&self) -> usize {
        0
    }
}

/// Trait for device file backend operations.
///
/// Implementors provide low-level read/write/ioctl semantics for registered
/// special-device operation tables.
pub trait DeviceFileOps: Send + Sync {
    /// Opens a device-backed file.
    fn open(&self, _inode: &VfsInode, _file: &mut VfsFileBuilder) -> VfsResult<()> {
        Ok(())
    }

    /// Returns whether this device provides a read callback.
    fn supports_read(&self) -> bool {
        false
    }

    /// Returns whether this device provides a write callback.
    fn supports_write(&self) -> bool {
        false
    }

    /// Reads data from the device at the specified offset.
    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    /// Writes data to the device at the specified offset.
    fn write(&self, _file: &VfsFile, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::InvalidInput)
    }
    /// Manipulates the underlying device parameters of special files.
    fn ioctl(&self, _file: &VfsFile, _cmd: u32, _arg: usize) -> VfsResult<usize> {
        Err(VfsError::NotATty)
    }

    /// Handle mmap for this device via the provided mapper.
    /// Default returns `ENODEV` (mmap not supported).
    fn mmap(&self, _file: &VfsFile, _mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        Err(VfsError::NoSuchDevice)
    }

    /// Polls an open device file.
    fn poll(&self, _file: &VfsFile) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    /// Registers a poll waiter for an open device file.
    fn register_poll(&self, _file: &VfsFile, _context: &mut Context<'_>, _events: IoEvents) {}

    /// Returns the inode flags used when a filesystem materializes this device node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::time::Duration;

    use unittest::def_test;

    use super::*;
    use crate::{
        GetattrQueryFlags, GetattrRequestMask, InodeOperations, Metadata, MetadataUpdate,
        MountIdmap, NodeFlags, NodePermission, NodeType, Umode, VfsInodeInit,
    };

    struct TestDeviceOps;

    impl DeviceFileOps for TestDeviceOps {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
            Ok(0)
        }

        fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
            Ok(buf.len())
        }
    }

    struct TestChrdevInode {
        rdev: DeviceId,
    }

    impl InodeOperations for TestChrdevInode {
        fn getattr(
            &self,
            _idmap: &MountIdmap,
            _path: Option<&crate::Path>,
            _request_mask: GetattrRequestMask,
            _query_flags: GetattrQueryFlags,
        ) -> VfsResult<Metadata> {
            Ok(Metadata {
                device: 0,
                inode: 7,
                nlink: 1,
                mode: Umode::new(
                    NodeType::CharacterDevice,
                    NodePermission::from_bits_truncate(0o666),
                ),
                uid: 0,
                gid: 0,
                size: 0,
                block_size: 0,
                blocks: 0,
                rdev: self.rdev,
                atime: Duration::ZERO,
                mtime: Duration::ZERO,
                ctime: Duration::ZERO,
            })
        }

        fn setattr(
            &self,
            _idmap: &MountIdmap,
            _dentry: &crate::Dentry,
            _update: MetadataUpdate,
        ) -> VfsResult<()> {
            Ok(())
        }
    }

    #[def_test]
    fn cdev_del_clears_inode_cdev() {
        let device = DeviceId::new(240, 1);
        let _ = cdev_del(device);

        let ops: Arc<dyn DeviceFileOps> = Arc::new(TestDeviceOps);
        cdev_add(device, ops.clone());
        let node = Arc::new(TestChrdevInode { rdev: device });
        let init = VfsInodeInit::new(
            99,
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, device);
        let inode = VfsInode::new_special(node, NodeFlags::empty(), init);

        let cached = CHRDEV_REGISTRY
            .get(&inode)
            .expect("registered cdev is found");
        assert!(Arc::ptr_eq(&cached, &ops));
        assert!(inode.character_device().is_some());

        let removed = cdev_del(device).expect("registered cdev is removed");
        assert!(Arc::ptr_eq(&removed, &ops));
        assert!(inode.character_device().is_none());
    }
}
