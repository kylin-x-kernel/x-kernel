// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping callback.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use block::{BlockDevice, BlockOpenMode, DriverError};
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

/// One registration identity; inode caches hold only weak references to it.
pub(super) struct ChrdevEntry {
    first: DeviceId,
    last_minor: u32,
    ops: Arc<dyn DeviceFileOps>,
    is_registered: AtomicBool,
}

impl ChrdevEntry {
    fn new(first: DeviceId, count: u32, ops: Arc<dyn DeviceFileOps>) -> VfsResult<Self> {
        let last_minor = count
            .checked_sub(1)
            .and_then(|offset| first.minor().checked_add(offset))
            .ok_or(VfsError::InvalidInput)?;
        Ok(Self {
            first,
            last_minor,
            ops,
            is_registered: AtomicBool::new(true),
        })
    }

    fn live_operations(&self) -> Option<Arc<dyn DeviceFileOps>> {
        // Removal publishes retirement before the range can be reused. The bit
        // never returns to true: a subsequent registration has a new identity.
        self.is_registered
            .load(Ordering::Acquire)
            .then(|| self.ops.clone())
    }
}

struct ChrdevRegistry {
    entries: BTreeMap<(u32, u32), Arc<ChrdevEntry>>,
}

impl ChrdevRegistry {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn add(&mut self, entry: &Arc<ChrdevEntry>) -> VfsResult<()> {
        let major = entry.first.major();
        if self
            .entries
            .range(..=(major, entry.last_minor))
            .next_back()
            .is_some_and(|(_, previous)| {
                previous.first.major() == major && previous.last_minor >= entry.first.minor()
            })
        {
            return Err(VfsError::ResourceBusy);
        }
        self.entries
            .insert((major, entry.first.minor()), entry.clone());
        Ok(())
    }

    fn del(&mut self, first: DeviceId, count: u32) -> Option<Arc<ChrdevEntry>> {
        let last_minor = first.minor().checked_add(count.checked_sub(1)?)?;
        let key = (first.major(), first.minor());
        if self.entries.get(&key)?.last_minor != last_minor {
            return None;
        }
        let entry = self.entries.remove(&key)?;
        entry.is_registered.store(false, Ordering::Release);
        Some(entry)
    }

    fn get(&self, device: DeviceId) -> Option<Arc<ChrdevEntry>> {
        let (_, entry) = self
            .entries
            .range(..=(device.major(), device.minor()))
            .next_back()?;
        (entry.first.major() == device.major() && device.minor() <= entry.last_minor)
            .then(|| entry.clone())
    }
}

static_lock! {
    static CHRDEV_MAP: Mutex<ChrdevRegistry> = Mutex::new(ChrdevRegistry::new());
}

fn character_operations(inode: &VfsInode) -> Option<Arc<dyn DeviceFileOps>> {
    if let Some(entry) = inode.character_device()
        && let Some(ops) = entry.live_operations()
    {
        return Some(ops);
    }
    // Selection linearizes under the registry lock. Removal does not revoke an
    // already selected backend; drivers enforce their own open-admission rules.
    let entry = CHRDEV_MAP.lock().get(inode.rdev())?;
    inode.set_character_device(Arc::downgrade(&entry));
    Some(entry.ops.clone())
}

/// Registers a driver for a nonempty minor range within one major.
///
/// Requires sleepable task context. A single device uses `count == 1`.
/// No registration replaces or overrides another. The caller owns the range
/// until a matching `cdev_del(first, count)`; device-node lifetime is separate.
///
/// # Errors
/// Returns `InvalidInput` for an empty or overflowing minor range, and
/// `ResourceBusy` for any overlap, including a duplicate registration.
pub fn cdev_add(first: DeviceId, count: u32, ops: Arc<dyn DeviceFileOps>) -> VfsResult<()> {
    let entry = Arc::new(ChrdevEntry::new(first, count, ops)?);
    let result = CHRDEV_MAP.lock().add(&entry);
    // In particular, a rejected backend's destructor runs after unlocking.
    drop(entry);
    result
}

/// Removes exactly the registered range and returns its backend.
///
/// Requires sleepable task context. Only the range owner may unregister it;
/// a mismatched start/count leaves the registry unchanged and returns `None`.
/// Cached inode registrations become invalid before the number can be reused.
/// Already selected/open backends remain owned by their callers and files.
pub fn cdev_del(first: DeviceId, count: u32) -> Option<Arc<dyn DeviceFileOps>> {
    let entry = CHRDEV_MAP.lock().del(first, count)?;
    Some(entry.ops.clone())
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
        let file_inode = file.inode()?;
        debug_assert!(core::ptr::eq(inode, file_inode.as_ref()));
        let ops =
            character_operations(file_inode).ok_or_else(|| VfsError::from(LinuxError::ENXIO))?;
        open_character_device(inode, file, ops)
    }
}

impl FileOperations for DefaultBlkdevFileOperations {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let device = block::lookup_block_device(inode.rdev())
            .ok_or_else(|| VfsError::from(LinuxError::ENXIO))?;
        device.open(block_open_mode(file.mode()))
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
            device.release();
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
            _ => self.driver_ioctl(block_open_mode(file.mode()), cmd, arg),
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

#[unittest::mod_test]
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
        // An inode-local registration exercises cached dispatch without
        // publishing a number or disturbing another test's registration.
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
        let entry = Arc::new(ChrdevEntry::new(device, 1, ops).unwrap());
        inode.set_character_device(Arc::downgrade(&entry));
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

    fn test_character_inode(device: DeviceId) -> Arc<VfsInode> {
        let init = VfsInodeInit::new(
            crate::get_next_ino(),
            0,
            Umode::new(
                NodeType::CharacterDevice,
                NodePermission::from_bits_truncate(0o666),
            ),
        )
        .with_owner_links_and_rdev(0, 0, 1, device);
        VfsInode::new_special(
            Arc::new(TestChrdevInode { rdev: device }),
            NodeFlags::empty(),
            init,
        )
    }

    fn add_test_range(
        registry: &mut ChrdevRegistry,
        first: DeviceId,
        count: u32,
        ops: &Arc<dyn DeviceFileOps>,
    ) -> VfsResult<()> {
        let entry = Arc::new(ChrdevEntry::new(first, count, ops.clone())?);
        registry.add(&entry)
    }

    #[def_test]
    fn cdev_ranges_validate_bounds_overlap_and_exact_removal() {
        let mut registry = ChrdevRegistry::new();
        let ops: Arc<dyn DeviceFileOps> = Arc::new(TestDeviceOps);
        assert_eq!(
            add_test_range(&mut registry, DeviceId::new(240, 10), 0, &ops),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            add_test_range(&mut registry, DeviceId::new(240, u32::MAX), 2, &ops),
            Err(VfsError::InvalidInput)
        );
        add_test_range(&mut registry, DeviceId::new(240, u32::MAX), 1, &ops).unwrap();
        assert!(registry.get(DeviceId::new(240, u32::MAX)).is_some());
        add_test_range(&mut registry, DeviceId::new(240, 10), 3, &ops).unwrap();
        for minor in [10, 11, 12] {
            assert!(Arc::ptr_eq(
                &registry.get(DeviceId::new(240, minor)).unwrap().ops,
                &ops
            ));
        }
        for minor in [9, 13] {
            assert!(registry.get(DeviceId::new(240, minor)).is_none());
        }
        assert!(registry.get(DeviceId::new(241, 10)).is_none());
        for (first, count) in [(9, 2), (10, 3), (11, 1), (12, 2), (9, 5)] {
            assert_eq!(
                add_test_range(&mut registry, DeviceId::new(240, first), count, &ops),
                Err(VfsError::ResourceBusy)
            );
        }
        add_test_range(&mut registry, DeviceId::new(240, 13), 1, &ops).unwrap();
        add_test_range(&mut registry, DeviceId::new(241, 10), 3, &ops).unwrap();
        for (first, count) in [(10, 0), (10, 2), (10, 4), (11, 2)] {
            assert!(registry.del(DeviceId::new(240, first), count).is_none());
        }
        let entry = registry.get(DeviceId::new(240, 11)).unwrap();
        drop(registry.del(DeviceId::new(240, 10), 3).unwrap());
        assert!(entry.live_operations().is_none());
        assert!(registry.get(DeviceId::new(240, 11)).is_none());
        assert!(registry.get(DeviceId::new(240, 13)).is_some());
        assert!(registry.get(DeviceId::new(241, 11)).is_some());
    }

    struct TestRegistration {
        first: DeviceId,
        count: u32,
    }

    impl TestRegistration {
        fn new(first: DeviceId, count: u32, ops: Arc<dyn DeviceFileOps>) -> Self {
            cdev_add(first, count, ops).unwrap();
            Self { first, count }
        }
    }

    impl Drop for TestRegistration {
        fn drop(&mut self) {
            drop(cdev_del(self.first, self.count));
        }
    }

    #[def_test(serial)]
    fn cdev_inode_cache_distinguishes_registrations_of_the_same_backend() {
        let first = DeviceId::new(242, 10);
        let ops: Arc<dyn DeviceFileOps> = Arc::new(TestDeviceOps);
        let _registration = TestRegistration::new(first, 3, ops.clone());
        let inode = test_character_inode(DeviceId::new(242, 11));
        assert!(Arc::ptr_eq(&character_operations(&inode).unwrap(), &ops));
        let old_entry = inode.character_device().unwrap();
        drop(cdev_del(first, 3).unwrap());
        assert!(old_entry.live_operations().is_none());
        assert!(character_operations(&inode).is_none());

        // Even reusing the same backend Arc must create a distinct registration.
        cdev_add(first, 3, ops.clone()).unwrap();
        assert!(Arc::ptr_eq(&character_operations(&inode).unwrap(), &ops));
        let new_entry = inode.character_device().unwrap();
        assert!(!Arc::ptr_eq(&old_entry, &new_entry));
        assert!(old_entry.live_operations().is_none());
        drop(old_entry);
        assert!(Arc::ptr_eq(&character_operations(&inode).unwrap(), &ops));
    }

    #[def_test(serial)]
    fn cdev_removal_preserves_open_files_but_not_inode_backend_ownership() {
        let first = DeviceId::new(242, 20);
        let backend = CharacterBackend::new(true, true, false);
        let weak = Arc::downgrade(&backend);
        let registration = TestRegistration::new(first, 2, backend.clone());
        let inode = test_character_inode(DeviceId::new(242, 21));
        let mount = crate::Mount::new_root(&crate::nullfs::new_superblock());
        let dentry = crate::Dentry::new_file_from_inode(inode.clone(), None, String::from("range"));
        let path = crate::Path::new(mount, dentry);
        let file = VfsFileBuilder::allocate(crate::OpenFlags::READ_WRITE, kcred::initial_cred())
            .unwrap()
            .vfs_open(path.clone())
            .unwrap();
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
        drop(registration);
        assert!(character_operations(&inode).is_none());
        assert_eq!(
            VfsFileBuilder::allocate(crate::OpenFlags::READ_WRITE, kcred::initial_cred())
                .unwrap()
                .vfs_open(path)
                .err(),
            Some(VfsError::from(LinuxError::ENXIO))
        );
        backend.is_ready.store(true, Ordering::Relaxed);
        assert!(file.poll().contains(IoEvents::IN));
        drop(file);
        assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
        drop(backend);
        assert!(weak.upgrade().is_none());
        // A live inode does not retain either a retired backend or its strong entry.
        assert!(inode.character_device().is_none());
    }

    #[def_test(serial)]
    fn cdev_callbacks_and_backend_destruction_run_without_registry_lock() {
        struct Backend;
        impl DeviceFileOps for Backend {
            fn open(&self, _: &VfsInode, _: &mut VfsFileBuilder) -> VfsResult<()> {
                assert!(CHRDEV_MAP.try_lock().is_some());
                Err(VfsError::Io)
            }
        }
        impl Drop for Backend {
            fn drop(&mut self) {
                assert!(CHRDEV_MAP.try_lock().is_some());
            }
        }
        let first = DeviceId::new(242, 30);
        let registration = TestRegistration::new(first, 1, Arc::new(Backend));
        assert_eq!(
            cdev_add(first, 1, Arc::new(Backend)),
            Err(VfsError::ResourceBusy)
        );
        assert_eq!(
            cdev_add(first, 0, Arc::new(Backend)),
            Err(VfsError::InvalidInput)
        );
        let inode = test_character_inode(first);
        let mount = crate::Mount::new_root(&crate::nullfs::new_superblock());
        let dentry = crate::Dentry::new_file_from_inode(inode, None, String::from("range"));
        let path = crate::Path::new(mount, dentry);
        assert_eq!(
            VfsFileBuilder::allocate(crate::OpenFlags::READ_WRITE, kcred::initial_cred())
                .unwrap()
                .vfs_open(path)
                .err(),
            Some(VfsError::Io)
        );
        drop(registration);
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

    impl block::BlockDeviceOperations for MemoryBlockDevice {
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
        let disk = block::Gendisk::alloc(
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
        .expect("valid KVFS test disk");
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
            let disk = block::Gendisk::alloc(
                String::from("kvfs-extent-test"),
                244,
                0,
                1,
                Box::new(backend.clone()),
            )
            .expect("valid extent test disk");
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
