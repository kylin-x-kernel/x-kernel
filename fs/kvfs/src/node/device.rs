// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping callback.

use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use block::{BlockDevice, BlockOpenMode, DriverError};
use kerrno::{KError, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError};
use ksync::static_lock;
use memaddr::PhysAddrRange;
use osvm::VirtPtr;

use crate::{
    DeviceId, FMode, FallocateFlags, FileOperations, Mutex, NodeFlags, VfsError, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult,
};

mod registration;
use registration::CharacterDeviceIdentity;
pub use registration::{
    CharacterDeviceRegistration, cdev_add_named, registered_character_devices,
    subscribe_character_devices,
};

/// One registration identity; inode caches hold only weak references to it.
pub(super) struct ChrdevEntry {
    first: DeviceId,
    last_minor: u32,
    ops: Arc<dyn DeviceFileOps>,
    is_registered: AtomicBool,
    identity: Option<CharacterDeviceIdentity>,
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
            identity: None,
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
    subscribers: Vec<Arc<dyn Fn() + Send + Sync>>,
}

impl ChrdevRegistry {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            subscribers: Vec::new(),
        }
    }

    fn add(&mut self, entry: &Arc<ChrdevEntry>) -> VfsResult<()> {
        // Named insertion is a cold publication path. A second live number
        // with the same class/name would collide in every filesystem view.
        if let Some(identity) = &entry.identity
            && self.entries.values().any(|existing| {
                existing
                    .identity
                    .as_ref()
                    .is_some_and(|other| other.matches(identity))
            })
        {
            return Err(VfsError::ResourceBusy);
        }
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
    let (entry, subscribers) = {
        let mut registry = CHRDEV_MAP.lock();
        let entry = registry.del(first, count)?;
        let subscribers = if entry.identity.is_some() {
            registry.subscribers.clone()
        } else {
            Vec::new()
        };
        (entry, subscribers)
    };
    // Projection callbacks may acquire filesystem locks; never run them under
    // the registry lock. The retired identity is already unavailable.
    for subscriber in subscribers {
        subscriber();
    }
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

    fn ioctl(&self, file: &VfsFile, cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.ops.ioctl(file, cred, cmd, arg)
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

    fn ioctl(&self, file: &VfsFile, cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        let device = file_block_device(file)?;
        DeviceFileOps::ioctl(device.as_ref(), file, cred, cmd, arg)
    }

    fn fallocate(
        &self,
        file: &VfsFile,
        flags: FallocateFlags,
        offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        block_device_fallocate(file_block_device(file)?.as_ref(), flags, offset, len)
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

/// Mirrors Linux `blkdev_fallocate` (block/fops.c): only the zeroing modes
/// are supported, and the mode mask, capacity and logical-block alignment are
/// all validated before any media write. Generic file emulation must not run
/// against raw devices — an unaligned range is `EINVAL`, never a silent
/// partial-block write.
fn block_device_fallocate(
    device: &BlockDevice,
    flags: FallocateFlags,
    start: u64,
    mut len: u64,
) -> VfsResult<()> {
    use FallocateFlags as F;

    const ZERO_CHUNK_BYTES: usize = 64 * 1024;
    let supported = F::KEEP_SIZE | F::PUNCH_HOLE | F::ZERO_RANGE;
    if !supported.contains(flags) {
        return Err(VfsError::from(LinuxError::EOPNOTSUPP));
    }
    let capacity = device.size();
    if start >= capacity {
        return Err(VfsError::from(LinuxError::EINVAL));
    }
    let requested_end = start
        .checked_add(len)
        .ok_or_else(|| VfsError::from(LinuxError::EFBIG))?;
    let mut clamped = false;
    if requested_end > capacity {
        if !flags.contains(F::KEEP_SIZE) {
            return Err(VfsError::from(LinuxError::EINVAL));
        }
        len = capacity - start;
        clamped = true;
    }
    let block_size = device.block_size() as u64;
    if block_size == 0 {
        return Err(VfsError::from(LinuxError::ENXIO));
    }
    if (start | len) & (block_size - 1) != 0 {
        // Unaligned requests are EINVAL. The single exception is a KEEP_SIZE
        // clamp against a capacity that is not a logical-block multiple:
        // zero-to-end keeps the aligned prefix of the clamped remainder,
        // provided the start itself is aligned and at least one full block
        // remains after flooring. Everything else — an unaligned start, or
        // an unaligned length that was never clamped — stays EINVAL instead
        // of silently rewriting partial blocks.
        if !clamped || start & (block_size - 1) != 0 {
            return Err(VfsError::from(LinuxError::EINVAL));
        }
        len -= len & (block_size - 1);
        if len == 0 {
            return Err(VfsError::from(LinuxError::EINVAL));
        }
    }
    // Only the zeroing modes reach the media (Linux switch in
    // blkdev_fallocate). ZERO_RANGE maps to BLKDEV_ZERO_NOUNMAP: a plain
    // write-zeroes fallback is allowed, which is what the loop below does.
    //
    // PUNCH_HOLE maps to BLKDEV_ZERO_NOFALLBACK: Linux succeeds when the
    // device provides write-zeroes/discard offload and fails with EOPNOTSUPP
    // only otherwise. The `BlockDeviceOperations` contract currently defines
    // neither operation, so this kernel deliberately narrows punch hole to
    // EOPNOTSUPP for every backend — reported as a missing capability, not
    // as Linux semantics — rather than silently rewriting the medium with
    // software zeros.
    let is_zero_range = flags == F::ZERO_RANGE || flags == F::ZERO_RANGE | F::KEEP_SIZE;
    if !is_zero_range {
        return Err(VfsError::from(LinuxError::EOPNOTSUPP));
    }

    let zeros = vec![0u8; ZERO_CHUNK_BYTES];
    let end = start + len;
    let mut pos = start;
    while pos < end {
        let chunk = (end - pos).min(zeros.len() as u64) as usize;
        // Honor short writes: a concurrent capacity change (loop detach)
        // must surface as an error instead of reporting a full zeroing.
        let written = write_block_device(device, &zeros[..chunk], pos)?;
        if written == 0 {
            return Err(KError::Io);
        }
        pos += written as u64;
    }
    Ok(())
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

    fn ioctl(&self, file: &VfsFile, cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        BlockDevice::ioctl(
            self,
            block_open_mode(file.mode()),
            cmd,
            arg,
            cred.has_sys_admin(),
            || Ok(i32::from_ne_bytes((arg as *const [u8; 4]).read_vm()?)),
            |bytes| Ok(osvm::write_vm_bytes(arg as *mut u8, bytes)?),
        )
    }

    // Block-special inodes have no block-backed address-space operations yet.
    // Keep the default ENODEV instead of installing an unusable file mapping.
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
    /// Manipulates device parameters using the current caller's credentials.
    /// `cred` is the current subjective snapshot, not the opening credential.
    fn ioctl(
        &self,
        _file: &VfsFile,
        _cred: &kcred::Cred,
        _cmd: u32,
        _arg: usize,
    ) -> VfsResult<usize> {
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

unittest::path_mod_test!(tests, "../../ut-tests/node_device.rs");
