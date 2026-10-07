// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping callback.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use block::{BlockDevice, BlockDeviceOperations, BlockOpenMode, DriverError};
use hashbrown::HashMap;
use kerrno::{KError, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError};
use ksync::static_lock;
use linux_raw_sys::ioctl::{BLKGETSIZE, BLKGETSIZE64, BLKROGET, BLKROSET};
use memaddr::PhysAddrRange;
use osvm::{VirtMutPtr, VirtPtr};

use crate::{
    DeviceId, FMode, FileOperations, Mutex, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode,
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

static_lock! {
    static CHRDEV_MAP: Mutex<Option<HashMap<u64, ChrdevEntry>>> = Mutex::new(None);
}

struct ChrdevRegistry;

impl ChrdevRegistry {
    fn with<R>(&self, f: impl FnOnce(&mut HashMap<u64, ChrdevEntry>) -> R) -> R {
        let mut registry = CHRDEV_MAP.lock();
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
        open_character_device(inode, file, ops)
    }
}

static CHRDEV_REGISTRY: ChrdevRegistry = ChrdevRegistry;

/// Register a character-device operation table for a device number.
pub fn cdev_add(device: DeviceId, ops: Arc<dyn DeviceFileOps>) {
    CHRDEV_REGISTRY.add(device, ops);
}

/// Remove a character-device operation table for a device number.
pub fn cdev_del(device: DeviceId) -> Option<Arc<dyn DeviceFileOps>> {
    CHRDEV_REGISTRY.del(device)
}

/// Installs character-device dispatch and opens the selected backend once.
///
/// Called in sleepable task context during file open, including multiplexer
/// replacement. This does not register a device number. The backend must not
/// already have been opened for this builder; VFS does not reopen a replacement
/// table. After successful file publication, final file drop forwards release
/// to the installed backend. No registry lock is held across the callback.
///
/// # Errors
/// Propagates the backend's open error. On failure the caller must abort this
/// open; the backend is responsible for rolling back its incomplete setup.
pub fn open_character_device(
    inode: &VfsInode,
    file: &mut VfsFileBuilder,
    ops: Arc<dyn DeviceFileOps>,
) -> VfsResult<()> {
    let fops = Arc::new(ChrdevFileOperations::new(ops));
    file.replace_fops(fops.clone());
    fops.open(inode, file)
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
    fn fsync(&self, _file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }

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

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        self.ops.release(inode, file)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        self.ops.poll(file)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.ops.register_poll(file, context, events)
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
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let device = block::lookup_block_device(inode.rdev())
            .ok_or_else(|| VfsError::from(LinuxError::ENXIO))?;
        BlockDeviceOperations::open(
            device.as_ref(),
            device.as_ref(),
            block_open_mode(file.mode()),
        )
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        file_block_device(file)?.read(file, buf, offset)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        file_block_device(file)?.write(file, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        let device = file_block_device(file)?;
        DeviceFileOps::ioctl(device.as_ref(), file, cmd, arg)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        file_block_device(file)?.mmap(file, mapper)
    }

    fn release(&self, inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        if let Some(device) = block::lookup_block_device(inode.rdev()) {
            BlockDeviceOperations::release(device.as_ref(), device.as_ref());
        }
        Ok(())
    }

    fn fsync(&self, file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        file_block_device(file)?.flush().map_err(map_block_error)
    }
}

fn block_open_mode(mode: FMode) -> BlockOpenMode {
    let mut block_mode = BlockOpenMode::empty();
    if mode.contains(FMode::READ) {
        block_mode.insert(BlockOpenMode::READ);
    }
    if mode.contains(FMode::WRITE) {
        block_mode.insert(BlockOpenMode::WRITE);
    }
    block_mode
}

fn file_block_device(file: &VfsFile) -> VfsResult<Arc<BlockDevice>> {
    block::lookup_block_device(file.inode().rdev()).ok_or_else(|| VfsError::from(LinuxError::ENXIO))
}

fn map_block_error(error: DriverError) -> VfsError {
    match error {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::BadState => KError::BadState,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        DriverError::NoMemory => KError::NoMemory,
        DriverError::ReadOnly => KError::OperationNotPermitted,
        DriverError::ResourceBusy => KError::ResourceBusy,
        DriverError::Unsupported => KError::OperationNotSupported,
    }
}

fn read_block_device(device: &BlockDevice, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let total_bytes = device.size();
    if offset >= total_bytes {
        return Ok(0);
    }

    let block_size = device.block_size();
    let length = core::cmp::min(buf.len() as u64, total_bytes - offset) as usize;
    let output = &mut buf[..length];
    // Preserve an aligned caller extent: block backends already accept
    // contiguous blocks, so each sector need not incur its own completion wait.
    if offset.is_multiple_of(block_size as u64) && length.is_multiple_of(block_size) {
        device
            .read_block(offset / block_size as u64, output)
            .map_err(map_block_error)?;
        return Ok(length);
    }
    let first_block = offset / block_size as u64;
    let first_offset = (offset % block_size as u64) as usize;
    let extent = first_offset
        .checked_add(length)
        .ok_or(KError::InvalidInput)?;
    let blocks = extent.div_ceil(block_size);
    let mut copied = 0;
    let mut scratch = None;

    for index in 0..blocks {
        let block_offset = if index == 0 { first_offset } else { 0 };
        let copy_length = core::cmp::min(block_size - block_offset, length - copied);
        let block_id = first_block + index as u64;
        if block_offset == 0 && copy_length == block_size {
            device
                .read_block(block_id, &mut output[copied..copied + copy_length])
                .map_err(map_block_error)?;
        } else {
            let block = scratch.get_or_insert_with(|| alloc::vec![0u8; block_size]);
            device
                .read_block(block_id, block)
                .map_err(map_block_error)?;
            output[copied..copied + copy_length]
                .copy_from_slice(&block[block_offset..block_offset + copy_length]);
        }
        copied += copy_length;
    }
    Ok(length)
}

fn write_block_device(device: &BlockDevice, buf: &[u8], offset: u64) -> VfsResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    if device.is_read_only() {
        return Err(KError::OperationNotPermitted);
    }
    let total_bytes = device.size();
    if offset >= total_bytes {
        return Err(VfsError::StorageFull);
    }

    let block_size = device.block_size();
    let length = core::cmp::min(buf.len() as u64, total_bytes - offset) as usize;
    let input = &buf[..length];
    // Keep partial-sector writes on the read/modify/write path below.
    if offset.is_multiple_of(block_size as u64) && length.is_multiple_of(block_size) {
        device
            .write_block(offset / block_size as u64, input)
            .map_err(map_block_error)?;
        return Ok(length);
    }
    let first_block = offset / block_size as u64;
    let first_offset = (offset % block_size as u64) as usize;
    let extent = first_offset
        .checked_add(length)
        .ok_or(KError::InvalidInput)?;
    let blocks = extent.div_ceil(block_size);
    let mut copied = 0;
    let mut scratch = None;

    for index in 0..blocks {
        let block_offset = if index == 0 { first_offset } else { 0 };
        let copy_length = core::cmp::min(block_size - block_offset, length - copied);
        let block_id = first_block + index as u64;
        let result = if block_offset == 0 && copy_length == block_size {
            device.write_block(block_id, &input[copied..copied + copy_length])
        } else {
            let block = scratch.get_or_insert_with(|| alloc::vec![0u8; block_size]);
            device.read_block(block_id, block).and_then(|()| {
                block[block_offset..block_offset + copy_length]
                    .copy_from_slice(&input[copied..copied + copy_length]);
                device.write_block(block_id, block)
            })
        };
        if let Err(error) = result {
            return if copied == 0 {
                Err(map_block_error(error))
            } else {
                Ok(copied)
            };
        }
        copied += copy_length;
    }

    Ok(length)
}

impl DeviceFileOps for BlockDevice {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        read_block_device(self, buf, offset)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        write_block_device(self, buf, offset)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            BLKGETSIZE => {
                let sectors =
                    usize::try_from(self.size() / 512).map_err(|_| VfsError::FileTooLarge)?;
                (arg as *mut usize).write_vm(sectors)?;
                Ok(0)
            }
            BLKGETSIZE64 => {
                (arg as *mut u64).write_vm(self.size())?;
                Ok(0)
            }
            BLKROGET => {
                (arg as *mut u32).write_vm(self.is_read_only() as u32)?;
                Ok(0)
            }
            BLKROSET => {
                let read_only = (arg as *const u32).read_vm()?;
                if read_only > 1 {
                    return Err(KError::InvalidInput);
                }
                self.set_disk_read_only(read_only != 0)?;
                Ok(0)
            }
            _ => BlockDeviceOperations::ioctl(self, self, block_open_mode(file.mode()), cmd, arg),
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        mapper.map_file_backed()
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

    /// Called when an open reference to the device file is released (the last
    /// fd referring to this open file description is closed).
    ///
    /// Devices that bind resources to an open file — via per-fd private data
    /// installed in [`open`](Self::open) — use this hook to tear them down.
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> VfsResult<()> {
        Ok(())
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
    fn register_poll(
        &self,
        _file: &VfsFile,
        _context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Ok(())
    }

    /// Returns the inode flags used when a filesystem materializes this device node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use kpoll::{PollRegistrations, PollSet};
    use ksync::Mutex;
    use ktime_types::SystemTime;
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
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
            })
        }

        fn setattr(
            &self,
            _idmap: &MountIdmap,
            _dentry: &crate::Dentry,
            update: MetadataUpdate,
        ) -> VfsResult<MetadataUpdate> {
            Ok(update)
        }
    }

    struct CharacterBackend {
        can_read: bool,
        can_write: bool,
        fail_open: bool,
        is_ready: AtomicBool,
        reject_registration: AtomicBool,
        opens: AtomicUsize,
        releases: AtomicUsize,
        waiters: PollSet,
    }

    impl CharacterBackend {
        fn new(can_read: bool, can_write: bool, fail_open: bool) -> Arc<Self> {
            Arc::new(Self {
                can_read,
                can_write,
                fail_open,
                is_ready: AtomicBool::new(false),
                reject_registration: AtomicBool::new(false),
                opens: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
                waiters: PollSet::new(),
            })
        }
    }

    impl DeviceFileOps for CharacterBackend {
        fn open(&self, _: &VfsInode, _: &mut VfsFileBuilder) -> VfsResult<()> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            if self.fail_open {
                Err(VfsError::NoSuchDevice)
            } else {
                Ok(())
            }
        }

        fn release(&self, _: &VfsInode, _: &VfsFile) -> VfsResult<()> {
            self.releases.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn supports_read(&self) -> bool {
            self.can_read
        }

        fn supports_write(&self) -> bool {
            self.can_write
        }

        fn poll(&self, _: &VfsFile) -> IoEvents {
            if self.is_ready.load(Ordering::Acquire) {
                IoEvents::IN
            } else {
                IoEvents::empty()
            }
        }

        fn register_poll(
            &self,
            _: &VfsFile,
            context: &mut PollContext<'_>,
            events: IoEvents,
        ) -> Result<(), PollRegisterError> {
            if self.reject_registration.load(Ordering::Relaxed) {
                return Err(PollRegisterError::InvalidState);
            }
            if events.contains(IoEvents::IN) {
                context.register(&self.waiters)?;
            }
            Ok(())
        }
    }

    struct CharacterMultiplexer {
        backend: Arc<CharacterBackend>,
        opens: AtomicUsize,
        releases: AtomicUsize,
    }

    impl DeviceFileOps for CharacterMultiplexer {
        fn open(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            open_character_device(inode, file, self.backend.clone())
        }

        fn release(&self, _: &VfsInode, _: &VfsFile) -> VfsResult<()> {
            self.releases.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    fn open_test_character(ops: Arc<dyn DeviceFileOps>) -> VfsResult<Arc<VfsFile>> {
        // An inode-local backend exercises registry dispatch without publishing
        // a test device number or disturbing another test's registration.
        let device = DeviceId::default();
        let init = VfsInodeInit::new(
            99,
            0,
            Umode::new(NodeType::CharacterDevice, NodePermission::default()),
        );
        let inode = VfsInode::new_special(
            Arc::new(TestChrdevInode { rdev: device }),
            NodeFlags::empty(),
            init,
        );
        inode.set_character_device(ops);
        let mount = crate::Mount::new_root(&crate::nullfs::new_superblock());
        let dentry = crate::Dentry::new_file_from_inode(inode, None, String::from("device"));
        let path = crate::Path::new(mount, dentry);
        VfsFileBuilder::allocate(crate::OpenFlags::READ_WRITE, kcred::initial_cred())?
            .vfs_open(path)
    }

    #[def_test]
    fn character_replacement_opens_once_and_releases_only_final_backend() {
        for can_read in [false, true] {
            for can_write in [false, true] {
                let backend = CharacterBackend::new(can_read, can_write, false);
                let multiplexer = Arc::new(CharacterMultiplexer {
                    backend: backend.clone(),
                    opens: AtomicUsize::new(0),
                    releases: AtomicUsize::new(0),
                });
                let file = open_test_character(multiplexer.clone()).unwrap();
                assert_eq!(multiplexer.opens.load(Ordering::Relaxed), 1);
                assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
                assert_eq!(file.mode().contains(FMode::CAN_READ), can_read);
                assert_eq!(file.mode().contains(FMode::CAN_WRITE), can_write);
                let alias = file.clone();
                drop(file);
                assert_eq!(backend.releases.load(Ordering::Relaxed), 0);
                drop(alias);
                assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
                assert_eq!(multiplexer.releases.load(Ordering::Relaxed), 0);
            }
        }
        let backend = CharacterBackend::new(true, true, true);
        let multiplexer = Arc::new(CharacterMultiplexer {
            backend: backend.clone(),
            opens: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        });
        assert!(matches!(
            open_test_character(multiplexer.clone()),
            Err(VfsError::NoSuchDevice)
        ));
        assert_eq!(multiplexer.opens.load(Ordering::Relaxed), 1);
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 0);
        assert_eq!(multiplexer.releases.load(Ordering::Relaxed), 0);
    }

    struct CharacterWake(AtomicBool);

    impl alloc::task::Wake for CharacterWake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[def_test]
    fn character_adapter_forwards_readiness_event_mask_and_registration_error() {
        let backend = CharacterBackend::new(true, true, false);
        let file = open_test_character(backend.clone()).unwrap();
        assert!(file.poll().is_empty());
        let wake = Arc::new(CharacterWake(AtomicBool::new(false)));
        let waker = Waker::from(wake.clone());
        let cx = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        file.register_poll(&mut registrations.context(&cx), IoEvents::OUT)
            .unwrap();
        backend.waiters.wake();
        assert!(!wake.0.load(Ordering::Acquire));
        file.register_poll(&mut registrations.context(&cx), IoEvents::IN)
            .unwrap();
        backend.is_ready.store(true, Ordering::Release);
        backend.waiters.wake();
        assert!(wake.0.load(Ordering::Acquire));
        assert_eq!(file.poll().bits(), IoEvents::IN.bits());
        backend.is_ready.store(false, Ordering::Release);
        assert!(file.poll().is_empty());
        backend.reject_registration.store(true, Ordering::Relaxed);
        assert_eq!(
            file.register_poll(&mut registrations.context(&cx), IoEvents::IN),
            Err(PollRegisterError::InvalidState)
        );
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

    struct MemoryBlockDevice {
        storage: Arc<Mutex<Vec<u8>>>,
        block_size: usize,
        flushes: Arc<core::sync::atomic::AtomicUsize>,
        reads: Arc<core::sync::atomic::AtomicUsize>,
        writes: Arc<core::sync::atomic::AtomicUsize>,
        fail_io: Arc<core::sync::atomic::AtomicBool>,
        fail_after_write_bytes: core::sync::atomic::AtomicUsize,
    }

    impl BlockDeviceOperations for MemoryBlockDevice {
        fn num_blocks(&self) -> u64 {
            (self.storage.lock().len() / self.block_size) as u64
        }

        fn block_size(&self) -> usize {
            self.block_size
        }

        fn read_block(&self, block_id: u64, buf: &mut [u8]) -> block::DriverResult {
            self.reads
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if self.fail_io.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(DriverError::Io);
            }
            let start = block_id as usize * self.block_size;
            let end = start + buf.len();
            buf.copy_from_slice(&self.storage.lock()[start..end]);
            Ok(())
        }

        fn write_block(&self, block_id: u64, buf: &[u8]) -> block::DriverResult {
            self.writes
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if self.fail_io.load(core::sync::atomic::Ordering::Relaxed) {
                return Err(DriverError::Io);
            }
            let start = block_id as usize * self.block_size;
            let prefix = self
                .fail_after_write_bytes
                .load(core::sync::atomic::Ordering::Relaxed);
            if prefix != usize::MAX {
                let written = prefix.min(buf.len());
                self.storage.lock()[start..start + written].copy_from_slice(&buf[..written]);
                return Err(DriverError::Io);
            }
            let end = start + buf.len();
            self.storage.lock()[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&self) -> block::DriverResult {
            self.flushes
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[def_test(serial)]
    fn block_special_file_io_crosses_blocks_and_obeys_capacity() {
        const BLOCK_SIZE: usize = 512;
        const BLOCKS: usize = 4;
        let initial: Vec<u8> = (0..BLOCK_SIZE * BLOCKS)
            .map(|index| (index % 251) as u8)
            .collect();
        let storage = Arc::new(Mutex::new(initial.clone()));
        let flushes = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let reads = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let writes = Arc::new(core::sync::atomic::AtomicUsize::new(0));
        let fail_io = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let disk = Arc::new(
            block::Gendisk::new(
                String::from("kvfs-block-test"),
                243,
                0,
                1,
                Box::new(MemoryBlockDevice {
                    storage: storage.clone(),
                    block_size: BLOCK_SIZE,
                    flushes: flushes.clone(),
                    reads: reads.clone(),
                    writes: writes.clone(),
                    fail_io: fail_io.clone(),
                    fail_after_write_bytes: core::sync::atomic::AtomicUsize::new(usize::MAX),
                }),
            )
            .expect("valid KVFS test disk"),
        );
        let device = block::add_disk(disk.clone()).expect("publish KVFS test disk");

        let mut output = vec![0; 1024];
        assert_eq!(read_block_device(&device, &mut output, 256).unwrap(), 1024);
        assert_eq!(&output[..], &initial[256..1280]);

        let input = vec![0x5a; 1024];
        assert_eq!(write_block_device(&device, &input, 256).unwrap(), 1024);
        assert_eq!(&storage.lock()[256..1280], &input[..]);
        assert_eq!(flushes.load(core::sync::atomic::Ordering::Relaxed), 0);

        use core::sync::atomic::Ordering::Relaxed;
        reads.store(0, Relaxed);
        writes.store(0, Relaxed);
        let aligned = vec![0xa7; 1024];
        assert_eq!(write_block_device(&device, &aligned, 512).unwrap(), 1024);
        assert_eq!(writes.load(Relaxed), 1);
        assert_eq!(reads.load(Relaxed), 0);
        assert_eq!(&storage.lock()[512..1536], &aligned[..]);
        let mut aligned_read = vec![0; 1024];
        assert_eq!(
            read_block_device(&device, &mut aligned_read, 512).unwrap(),
            1024
        );
        assert_eq!(aligned_read, aligned);
        assert_eq!(reads.load(Relaxed), 1);

        let mut clipped = vec![0; 2048];
        assert_eq!(
            read_block_device(&device, &mut clipped, 1024).unwrap(),
            1024
        );
        assert_eq!(&clipped[..1024], &storage.lock()[1024..]);
        assert_eq!(reads.load(Relaxed), 2);
        assert_eq!(
            write_block_device(&device, &[0x39; 2048], 1024).unwrap(),
            1024
        );
        assert_eq!(writes.load(Relaxed), 2);

        fail_io.store(true, Relaxed);
        assert_eq!(
            read_block_device(&device, &mut aligned_read, 512).unwrap_err(),
            KError::Io
        );
        assert_eq!(
            write_block_device(&device, &aligned, 512).unwrap_err(),
            KError::Io
        );
        assert_eq!(reads.load(Relaxed), 3);
        assert_eq!(writes.load(Relaxed), 3);
        fail_io.store(false, Relaxed);

        let mut tail = vec![0; 1024];
        assert_eq!(
            read_block_device(&device, &mut tail, (BLOCK_SIZE * BLOCKS - 128) as u64).unwrap(),
            128
        );
        assert_eq!(
            write_block_device(&device, &[1], (BLOCK_SIZE * BLOCKS) as u64).unwrap_err(),
            VfsError::StorageFull
        );

        device
            .set_disk_read_only(true)
            .expect("set test disk read-only");
        let writes_before = writes.load(Relaxed);
        assert_eq!(
            write_block_device(&device, &[0; 1024], 0).unwrap_err(),
            VfsError::from(KError::OperationNotPermitted)
        );
        assert_eq!(writes.load(Relaxed), writes_before);
        assert_eq!(
            write_block_device(&device, &[1], 0).unwrap_err(),
            VfsError::from(KError::OperationNotPermitted)
        );

        block::del_gendisk(disk.device_number()).expect("remove KVFS test disk");
    }

    struct BlockIoFixture {
        device: Arc<BlockDevice>,
        backend: Arc<MemoryBlockDevice>,
    }

    impl BlockIoFixture {
        fn new(block_size: usize) -> Self {
            use core::sync::atomic::{AtomicBool, AtomicUsize};
            let backend = Arc::new(MemoryBlockDevice {
                storage: Arc::new(Mutex::new(vec![0x35; block_size * 8])),
                block_size,
                flushes: Arc::new(AtomicUsize::new(0)),
                reads: Arc::new(AtomicUsize::new(0)),
                writes: Arc::new(AtomicUsize::new(0)),
                fail_io: Arc::new(AtomicBool::new(false)),
                fail_after_write_bytes: AtomicUsize::new(usize::MAX),
            });
            let disk = Arc::new(
                block::Gendisk::new(
                    String::from("kvfs-extent-test"),
                    244,
                    0,
                    1,
                    Box::new(backend.clone()),
                )
                .expect("valid extent test disk"),
            );
            let device = block::add_disk(disk).expect("publish extent test disk");
            Self { device, backend }
        }
    }

    impl Drop for BlockIoFixture {
        fn drop(&mut self) {
            block::del_gendisk(self.device.disk().device_number())
                .expect("remove extent test disk");
        }
    }

    #[def_test(serial)]
    fn block_aligned_extents_preserve_data_and_neighbors() {
        use core::sync::atomic::Ordering::Relaxed;
        for block_size in [512, 4096] {
            let fixture = BlockIoFixture::new(block_size);
            let input: Vec<u8> = (0..block_size * 3).map(|i| (i % 251) as u8).collect();
            let mut output = vec![0; input.len()];
            assert_eq!(
                write_block_device(&fixture.device, &input, block_size as u64).unwrap(),
                input.len()
            );
            assert_eq!(fixture.backend.writes.load(Relaxed), 1);
            assert_eq!(fixture.backend.reads.load(Relaxed), 0);
            assert_eq!(
                read_block_device(&fixture.device, &mut output, block_size as u64).unwrap(),
                input.len()
            );
            assert_eq!(output, input);
            assert_eq!(fixture.backend.reads.load(Relaxed), 1);
            let storage = fixture.backend.storage.lock();
            assert!(storage[..block_size].iter().all(|&b| b == 0x35));
            assert!(storage[block_size * 4..].iter().all(|&b| b == 0x35));
            assert_eq!(fixture.backend.flushes.load(Relaxed), 0);
        }
    }

    #[def_test(serial)]
    fn block_empty_eof_and_capacity_clipping_do_not_touch_outside_view() {
        use core::sync::atomic::Ordering::Relaxed;
        let fixture = BlockIoFixture::new(512);
        fixture.device.set_capacity(4).unwrap();
        for offset in [0, 2048, u64::MAX] {
            assert_eq!(
                read_block_device(&fixture.device, &mut [], offset).unwrap(),
                0
            );
            assert_eq!(write_block_device(&fixture.device, &[], offset).unwrap(), 0);
        }
        for offset in [2048, 2049, u64::MAX] {
            assert_eq!(
                read_block_device(&fixture.device, &mut [0; 512], offset).unwrap(),
                0
            );
            assert_eq!(
                write_block_device(&fixture.device, &[1; 512], offset).unwrap_err(),
                KError::StorageFull
            );
        }
        assert_eq!(fixture.backend.reads.load(Relaxed), 0);
        assert_eq!(fixture.backend.writes.load(Relaxed), 0);
        let mut output = vec![0xa9; 2048];
        assert_eq!(
            read_block_device(&fixture.device, &mut output, 1024).unwrap(),
            1024
        );
        assert!(output[..1024].iter().all(|&b| b == 0x35));
        assert!(output[1024..].iter().all(|&b| b == 0xa9));
        assert_eq!(
            write_block_device(&fixture.device, &[0x71; 2048], 1024).unwrap(),
            1024
        );
        assert!(
            fixture.backend.storage.lock()[2048..]
                .iter()
                .all(|&b| b == 0x35)
        );
        assert_eq!(fixture.backend.reads.load(Relaxed), 1);
        assert_eq!(fixture.backend.writes.load(Relaxed), 1);
        fixture.device.set_disk_read_only(true).unwrap();
        assert_eq!(write_block_device(&fixture.device, &[], 0).unwrap(), 0);
        assert_eq!(
            write_block_device(&fixture.device, &[0; 1024], 0).unwrap_err(),
            KError::OperationNotPermitted
        );
        assert_eq!(fixture.backend.writes.load(Relaxed), 1);
    }

    #[def_test(serial)]
    fn block_unaligned_extents_preserve_read_modify_write_semantics() {
        for (offset, len) in [(1, 1), (0, 513), (511, 1024), (513, 1023)] {
            let fixture = BlockIoFixture::new(512);
            let input = vec![0x72; len];
            let mut output = vec![0; len];
            assert_eq!(
                write_block_device(&fixture.device, &input, offset as u64).unwrap(),
                len
            );
            assert_eq!(
                read_block_device(&fixture.device, &mut output, offset as u64).unwrap(),
                len
            );
            assert_eq!(output, input);
            let storage = fixture.backend.storage.lock();
            assert!(storage[..offset].iter().all(|&b| b == 0x35));
            assert!(storage[offset + len..].iter().all(|&b| b == 0x35));
        }
    }

    #[def_test(serial)]
    fn block_partial_backend_write_error_is_not_retried() {
        use core::sync::atomic::Ordering::Relaxed;
        let fixture = BlockIoFixture::new(512);
        fixture.backend.fail_after_write_bytes.store(512, Relaxed);
        assert_eq!(
            write_block_device(&fixture.device, &[0x72; 1536], 512).unwrap_err(),
            KError::Io
        );
        assert_eq!(fixture.backend.writes.load(Relaxed), 1);
        assert_eq!(fixture.backend.reads.load(Relaxed), 0);
        let storage = fixture.backend.storage.lock();
        assert!(storage[..512].iter().all(|&b| b == 0x35));
        assert!(storage[512..1024].iter().all(|&b| b == 0x72));
        assert!(storage[1024..].iter().all(|&b| b == 0x35));
    }

    #[def_test(serial)]
    fn block_read_failure_prevents_read_modify_write() {
        use core::sync::atomic::Ordering::Relaxed;
        let fixture = BlockIoFixture::new(512);
        fixture.backend.fail_io.store(true, Relaxed);
        let mut output = vec![0xa9; 1024];
        assert_eq!(
            read_block_device(&fixture.device, &mut output, 0).unwrap_err(),
            KError::Io
        );
        assert!(output.iter().all(|&b| b == 0xa9));
        assert_eq!(fixture.backend.reads.load(Relaxed), 1);
        assert_eq!(
            write_block_device(&fixture.device, &[0x72; 10], 1).unwrap_err(),
            KError::Io
        );
        assert_eq!(fixture.backend.reads.load(Relaxed), 2);
        assert_eq!(fixture.backend.writes.load(Relaxed), 0);
        assert!(fixture.backend.storage.lock().iter().all(|&b| b == 0x35));
    }
}
