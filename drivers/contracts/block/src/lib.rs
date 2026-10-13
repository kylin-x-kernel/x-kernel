// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style block-device core.
//!
//! Drivers provide [`BlockDeviceOperations`]. The block core owns persistent
//! disk identity in [`Gendisk`], request geometry in [`RequestQueue`], and the
//! whole-disk [`BlockDevice`] view. Capacity and offsets use fixed 512-byte
//! sectors, while backend I/O remains aligned to the queue's logical block size.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

extern crate alloc;

mod backing_dev;
pub mod completion;
mod ioctl;

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ops::Deref,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

pub use backing_dev::BackingDevInfo;
use kdevice::DeviceNumber;
use kerrno::{KError, KResult};
use klazy::Lazy;
use ksync::Mutex;

// #[cfg(feature = "bcm2835-sdhci")]
// pub mod bcm2835sdhci;

#[cfg(feature = "ramdisk")]
pub mod ramdisk;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_static;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_image;

mod queue;

// #[cfg(feature = "ahci")]
// pub mod ahci;
// #[cfg(feature = "sdmmc")]
// pub mod sdmmc;

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use queue::{QueueLimits, RequestQueue, SECTOR_SHIFT, SECTOR_SIZE};

bitflags::bitflags! {
    /// Access mode passed from an opened block-device file to the block driver.
    ///
    /// This is the object-oriented equivalent of Linux `blk_mode_t`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct BlockOpenMode: u32 {
        /// The opened file permits reads.
        const READ = 1 << 0;
        /// The opened file permits writes.
        const WRITE = 1 << 1;
    }
}

const DISK_STATE_ADMIN_READ_ONLY: usize = 1 << 0;

// Sequence allocation is independent of disk publication and I/O state.
static DISK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// I/O operations that a block storage backend must implement.
///
/// Device-number allocation and user-visible disk identity belong to
/// [`Gendisk`], not to this operations table.
pub trait BlockDeviceOperations: Send + Sync {
    /// The number of blocks in this storage device.
    ///
    /// The total size of the device is `num_blocks() * block_size()`.
    fn num_blocks(&self) -> u64;
    /// The size of each block in bytes.
    ///
    /// The value must be non-zero and must not change after the disk is
    /// published. Mutable media capacity is represented by
    /// [`BlockDevice::set_capacity`], not by changing this value.
    fn block_size(&self) -> usize;

    /// Returns whether the backend is inherently read-only.
    ///
    /// This capability must not change after the disk is published. The block
    /// core combines it with the administratively controlled read-only state.
    fn is_inherently_read_only(&self) -> bool {
        false
    }

    /// Reads blocked data from the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be read.
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult;

    /// Writes blocked data to the given block.
    ///
    /// The size of the buffer may exceed the block size, in which case multiple
    /// contiguous blocks will be written.
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult;

    /// Flushes the device to write all pending data to the storage.
    fn flush(&self) -> DriverResult;

    /// Opens a block-device view through this disk.
    fn open(&self, _device: &BlockDevice, _mode: BlockOpenMode) -> KResult<()> {
        Ok(())
    }

    /// Releases a previously opened block-device view.
    fn release(&self, _device: &BlockDevice) {}

    /// Handles a disk-specific ioctl not consumed by the generic block layer.
    ///
    /// `has_sys_admin` describes the current caller's CAP_SYS_ADMIN capability.
    /// Drivers enforce command-specific permissions using it and `mode`; an
    /// open device is not blanket authorization for every driver command.
    fn ioctl(
        &self,
        _device: &BlockDevice,
        _mode: BlockOpenMode,
        _cmd: u32,
        _arg: usize,
        _has_sys_admin: bool,
    ) -> KResult<usize> {
        Err(KError::NotATty)
    }

    /// Applies driver-specific work required before changing read-only state.
    fn set_read_only(&self, _device: &BlockDevice, _read_only: bool) -> KResult<()> {
        Ok(())
    }
}

impl<T: BlockDeviceOperations + ?Sized> BlockDeviceOperations for Arc<T> {
    fn num_blocks(&self) -> u64 {
        (**self).num_blocks()
    }

    fn block_size(&self) -> usize {
        (**self).block_size()
    }

    fn is_inherently_read_only(&self) -> bool {
        (**self).is_inherently_read_only()
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        (**self).read_block(block_id, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        (**self).write_block(block_id, buf)
    }

    fn flush(&self) -> DriverResult {
        (**self).flush()
    }

    fn open(&self, device: &BlockDevice, mode: BlockOpenMode) -> KResult<()> {
        (**self).open(device, mode)
    }

    fn release(&self, device: &BlockDevice) {
        (**self).release(device)
    }

    fn ioctl(
        &self,
        device: &BlockDevice,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
        has_sys_admin: bool,
    ) -> KResult<usize> {
        (**self).ioctl(device, mode, cmd, arg, has_sys_admin)
    }

    fn set_read_only(&self, device: &BlockDevice, read_only: bool) -> KResult<()> {
        (**self).set_read_only(device, read_only)
    }
}

/// A generic disk and its whole-device view.
///
/// This is the counterpart of Linux `struct gendisk`: disk identity, queue,
/// state and driver operations belong to this object. Its lifetime is owned
/// through `part0`, just as Linux `disk_to_dev()` uses `part0->bd_device`.
pub struct Gendisk {
    name: String,
    diskseq: AtomicU64,
    bdi: BackingDevInfo,
    major: u32,
    first_minor: u32,
    minors: u32,
    is_inherently_read_only: bool,
    state: AtomicUsize,
    operations: Box<dyn BlockDeviceOperations + Send + Sync>,
    queue: Arc<RequestQueue>,
    part0: Weak<BlockDevice>,
}

/// An owning disk reference, counted through its canonical whole device.
///
/// Cloning and dropping this reference correspond to acquiring and releasing
/// the Linux `disk_to_dev()` reference. It contains no disk state or open/claim
/// policy. Keeping the owning `Arc<Gendisk>` private prevents callers from
/// retaining the disk independently of its canonical `part0`.
#[derive(Clone)]
pub struct GendiskRef(Arc<BlockDevice>);

impl Deref for GendiskRef {
    type Target = Gendisk;

    fn deref(&self) -> &Gendisk {
        self.0.disk()
    }
}

impl Device for GendiskRef {
    fn name(&self) -> &str {
        self.deref().name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl Gendisk {
    /// Creates a disk from driver-private I/O operations.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] for an empty name, an invalid
    /// minor range, invalid queue geometry, or a byte capacity that overflows
    /// `u64`.
    pub fn alloc(
        name: String,
        major: u32,
        first_minor: u32,
        minors: u32,
        operations: Box<dyn BlockDeviceOperations + Send + Sync>,
    ) -> DriverResult<GendiskRef> {
        let limits = QueueLimits::new(operations.block_size())?;
        let capacity_sectors = operations
            .num_blocks()
            .checked_mul(limits.logical_block_sectors())
            .ok_or(DriverError::InvalidInput)?;
        if name.is_empty()
            || minors == 0
            || first_minor.checked_add(minors - 1).is_none()
            || capacity_sectors > u64::MAX >> SECTOR_SHIFT
        {
            return Err(DriverError::InvalidInput);
        }
        let is_inherently_read_only = operations.is_inherently_read_only();
        let queue = Arc::new(RequestQueue::new(limits));
        let part0 = Arc::new_cyclic(move |part0| {
            let disk = Arc::new(Self {
                name,
                bdi: BackingDevInfo::new(),
                diskseq: AtomicU64::new(DISK_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1),
                major,
                first_minor,
                minors,
                is_inherently_read_only,
                state: AtomicUsize::new(0),
                operations,
                queue,
                part0: part0.clone(),
            });
            BlockDevice::whole(disk, capacity_sectors)
        });
        Ok(GendiskRef(part0))
    }

    /// Returns the disk's shared backing-device policy (`gendisk::bdi`).
    pub fn backing_dev_info(&self) -> &BackingDevInfo {
        &self.bdi
    }

    /// Returns the block major assigned to this disk.
    pub fn major(&self) -> u32 {
        self.major
    }

    /// Returns the first minor assigned to this disk.
    pub fn first_minor(&self) -> u32 {
        self.first_minor
    }

    /// Returns the number of consecutive minors owned by this disk.
    pub fn minors(&self) -> u32 {
        self.minors
    }

    /// Returns the user-visible disk name (`disk_name`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the globally assigned disk/media sequence number.
    ///
    /// Re-publication and capacity changes retain this value. Drivers call
    /// [`Self::inc_diskseq`] after replacing media in an existing disk object.
    pub fn diskseq(&self) -> u64 {
        self.diskseq.load(Ordering::Relaxed)
    }

    /// Assigns a new global sequence number after a successful media change.
    ///
    /// The driver must serialize media changes. This identifies a media
    /// generation; it does not synchronize I/O or invalidate cached data.
    pub fn inc_diskseq(&self) {
        let sequence = DISK_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
        self.diskseq.store(sequence, Ordering::Relaxed);
    }

    /// Returns the request queue owned by this disk.
    pub fn queue(&self) -> &Arc<RequestQueue> {
        &self.queue
    }

    /// Returns the whole-disk block-device view.
    ///
    /// `part0` exists as soon as the disk is constructed; [`add_disk`] only
    /// publishes this same canonical object.
    pub fn part0(&self) -> Arc<BlockDevice> {
        // A disk borrow comes from either a live GendiskRef or its owning
        // BlockDevice. Both keep part0 alive; no standalone disk Arc escapes.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: a disk is only borrowed through a \
                      live GendiskRef or its owning BlockDevice, both of which keep part0 alive"
        )]
        self.part0
            .upgrade()
            .expect("a borrowed disk has a live part0")
    }
}

impl Device for Gendisk {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

/// A block-device view identified by one Linux-style device number.
///
/// This corresponds to Linux `struct block_device`. The current block core
/// creates the whole-disk view (`part0`); its start and length use the same
/// fixed 512-byte sector unit as Linux `bd_start_sect` and `bd_nr_sectors`.
pub struct BlockDevice {
    disk: Arc<Gendisk>,
    queue: Arc<RequestQueue>,
    device_number: DeviceNumber,
    start_sector: u64,
    num_sectors: AtomicU64,
    exclusive_holder: Mutex<bool>,
}

/// An exclusive holder claim on one canonical block-device object.
///
/// This is the ownership part of Linux's exclusive `bdev_open()`: while the
/// token is alive, another subsystem cannot establish a distinct holder for
/// the same [`BlockDevice`]. Dropping the token releases the claim.
pub struct BlockDeviceClaim {
    device: Arc<BlockDevice>,
    is_active: Mutex<bool>,
}

impl BlockDeviceClaim {
    /// Returns the canonical block-device object owned by this claim.
    pub fn device(&self) -> &Arc<BlockDevice> {
        &self.device
    }

    /// Releases the claim before the token itself is dropped.
    ///
    /// This is idempotent so an owning object's lifecycle can release block
    /// ownership at its logical death rather than waiting for its final `Arc`.
    pub fn release(&self) {
        let mut is_active = self.is_active.lock();
        if !*is_active {
            return;
        }
        let mut holder = self.device.exclusive_holder.lock();
        assert!(*holder, "an active block-device claim must own the holder");
        *holder = false;
        *is_active = false;
    }
}

impl Drop for BlockDeviceClaim {
    fn drop(&mut self) {
        self.release();
    }
}

impl BlockDevice {
    fn whole(disk: Arc<Gendisk>, capacity_sectors: u64) -> Self {
        let device_number = disk.device_number();
        Self {
            queue: disk.queue.clone(),
            disk,
            device_number,
            start_sector: 0,
            num_sectors: AtomicU64::new(capacity_sectors),
            exclusive_holder: Mutex::new(false),
        }
    }

    /// Claims this canonical block-device object for one exclusive holder.
    ///
    /// Filesystem superblocks retain the returned token for their complete
    /// lifetime, preventing a different filesystem instance from acquiring
    /// the same media concurrently.
    ///
    /// # Errors
    ///
    /// Returns [`KError::ResourceBusy`] while another holder owns the device.
    pub fn claim_exclusive(self: &Arc<Self>) -> KResult<BlockDeviceClaim> {
        let mut holder = self.exclusive_holder.lock();
        if *holder {
            return Err(KError::ResourceBusy);
        }
        *holder = true;
        Ok(BlockDeviceClaim {
            device: self.clone(),
            is_active: Mutex::new(true),
        })
    }

    /// Returns this view's device number (`bd_dev`).
    pub const fn device_number(&self) -> DeviceNumber {
        self.device_number
    }

    /// Returns this device's owning generic disk (`bd_disk`).
    ///
    /// The disk remains alive for this device's entire lifetime, including
    /// before publication and after removal from the registry.
    pub fn disk(&self) -> &Gendisk {
        &self.disk
    }

    /// Returns the request queue used by this block-device view (`bd_queue`).
    pub fn queue(&self) -> &Arc<RequestQueue> {
        &self.queue
    }

    /// Returns the first 512-byte sector visible through this device.
    pub const fn start_sector(&self) -> u64 {
        self.start_sector
    }

    /// Returns the number of 512-byte sectors visible through this device.
    pub fn num_sectors(&self) -> u64 {
        self.num_sectors.load(Ordering::Acquire)
    }

    /// Updates this view's capacity in 512-byte sectors.
    ///
    /// This is the object-oriented equivalent of Linux `set_capacity()` and is
    /// used by media whose size changes after publication, such as loop disks.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] when `capacity_sectors * 512`
    /// cannot be represented in `u64`.
    pub fn set_capacity(&self, capacity_sectors: u64) -> DriverResult<()> {
        if capacity_sectors > u64::MAX >> SECTOR_SHIFT {
            return Err(DriverError::InvalidInput);
        }
        self.num_sectors.store(capacity_sectors, Ordering::Release);
        Ok(())
    }

    /// Returns the queue's logical block size in bytes.
    pub fn block_size(&self) -> usize {
        self.queue.limits().logical_block_size() as usize
    }

    /// Returns this device's capacity in bytes.
    ///
    /// [`Gendisk::alloc`] and [`Self::set_capacity`] ensure that the sector
    /// capacity is representable in bytes.
    pub fn size(&self) -> u64 {
        self.num_sectors() << SECTOR_SHIFT
    }

    /// Returns whether inherent or administrative policy prohibits writes.
    pub fn is_read_only(&self) -> bool {
        self.disk.is_inherently_read_only
            || self.disk.state.load(Ordering::Acquire) & DISK_STATE_ADMIN_READ_ONLY != 0
    }

    /// Changes this disk's administrative read-only policy.
    /// Clearing it succeeds even when inherent read-only state remains set.
    ///
    /// The driver callback runs before the generic state is published, matching
    /// Linux `BLKROSET` ordering.
    ///
    /// # Errors
    ///
    /// Returns a driver callback error without changing the administrative state.
    pub fn set_disk_read_only(&self, is_read_only: bool) -> KResult<()> {
        self.disk.operations.set_read_only(self, is_read_only)?;
        if is_read_only {
            self.disk
                .state
                .fetch_or(DISK_STATE_ADMIN_READ_ONLY, Ordering::AcqRel);
        } else {
            self.disk
                .state
                .fetch_and(!DISK_STATE_ADMIN_READ_ONLY, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Returns the user-visible disk name through the owning `gendisk`.
    pub fn name(&self) -> &str {
        &self.disk.name
    }

    /// Reads one or more contiguous logical blocks from this device view.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] for an unaligned or out-of-range
    /// request. Backend failures are propagated unchanged.
    pub fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let backend_block = self.backend_block(block_id, buf.len())?;
        self.disk.operations.read_block(backend_block, buf)
    }

    /// Writes one or more contiguous logical blocks to this device view.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::ReadOnly`] when writes are prohibited and
    /// [`DriverError::InvalidInput`] for an unaligned or out-of-range request.
    /// Backend failures are propagated unchanged.
    pub fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        if self.is_read_only() {
            return Err(DriverError::ReadOnly);
        }
        let backend_block = self.backend_block(block_id, buf.len())?;
        self.disk.operations.write_block(backend_block, buf)
    }

    /// Flushes pending writes through the driver operations table.
    ///
    /// # Errors
    ///
    /// Propagates the backend flush error unchanged.
    pub fn flush(&self) -> DriverResult {
        self.disk.operations.flush()
    }

    /// Opens this block-device view with the requested access mode.
    ///
    /// # Errors
    ///
    /// Propagates the driver open error unchanged.
    pub fn open(&self, mode: BlockOpenMode) -> KResult<()> {
        self.disk.operations.open(self, mode)
    }

    /// Releases a previously opened block-device view.
    pub fn release(&self) {
        self.disk.operations.release(self);
    }

    /// Passes a device-specific ioctl to the driver operations table.
    ///
    /// Generic block ioctls are consumed before this method is called.
    ///
    /// # Errors
    ///
    /// Propagates the driver ioctl error unchanged.
    fn driver_ioctl(
        &self,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
        has_sys_admin: bool,
    ) -> KResult<usize> {
        self.disk
            .operations
            .ioctl(self, mode, cmd, arg, has_sys_admin)
    }

    fn backend_block(&self, block_id: u64, len: usize) -> DriverResult<u64> {
        let block_size = self.block_size();
        if !len.is_multiple_of(block_size) {
            return Err(DriverError::InvalidInput);
        }
        let logical_block_sectors = self.queue.limits().logical_block_sectors();
        let relative_sector = block_id
            .checked_mul(logical_block_sectors)
            .ok_or(DriverError::InvalidInput)?;
        let len_sectors =
            u64::try_from(len >> SECTOR_SHIFT).map_err(|_| DriverError::InvalidInput)?;
        let end_sector = relative_sector
            .checked_add(len_sectors)
            .ok_or(DriverError::InvalidInput)?;
        let capacity_sectors = self.num_sectors();
        if relative_sector > capacity_sectors || end_sector > capacity_sectors {
            return Err(DriverError::InvalidInput);
        }
        let backend_sector = self
            .start_sector
            .checked_add(relative_sector)
            .ok_or(DriverError::InvalidInput)?;
        if !backend_sector.is_multiple_of(logical_block_sectors) {
            return Err(DriverError::InvalidInput);
        }
        Ok(backend_sector / logical_block_sectors)
    }
}

impl Device for BlockDevice {
    fn name(&self) -> &str {
        &self.disk.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

// kext4 still consumes the synchronous operations interface until its Bio
// migration. This derived view delegates to the canonical `BlockDevice`
// methods and owns no geometry or capacity state of its own.
impl BlockDeviceOperations for BlockDevice {
    fn num_blocks(&self) -> u64 {
        self.num_sectors() / self.queue.limits().logical_block_sectors()
    }

    fn block_size(&self) -> usize {
        BlockDevice::block_size(self)
    }

    fn is_inherently_read_only(&self) -> bool {
        self.disk.is_inherently_read_only
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        BlockDevice::read_block(self, block_id, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        BlockDevice::write_block(self, block_id, buf)
    }

    fn flush(&self) -> DriverResult {
        BlockDevice::flush(self)
    }

    fn open(&self, _device: &BlockDevice, mode: BlockOpenMode) -> KResult<()> {
        BlockDevice::open(self, mode)
    }

    fn release(&self, _device: &BlockDevice) {
        BlockDevice::release(self);
    }

    fn ioctl(
        &self,
        _device: &BlockDevice,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
        has_sys_admin: bool,
    ) -> KResult<usize> {
        self.driver_ioctl(mode, cmd, arg, has_sys_admin)
    }

    fn set_read_only(&self, _device: &BlockDevice, read_only: bool) -> KResult<()> {
        self.set_disk_read_only(read_only)
    }
}

impl Gendisk {
    /// Returns the device number assigned to the whole disk.
    pub fn device_number(&self) -> DeviceNumber {
        DeviceNumber::new(self.major, self.first_minor)
    }
}

static BLOCK_DEVICES: Lazy<Mutex<BTreeMap<DeviceNumber, GendiskRef>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

/// Publishes a generic disk's existing whole-device `part0` view.
///
/// # Errors
///
/// Returns [`DriverError::InvalidInput`] when no major has been assigned, or
/// [`DriverError::AlreadyExists`] when the device-number range conflicts with
/// an existing disk.
pub fn add_disk(disk: GendiskRef) -> DriverResult<Arc<BlockDevice>> {
    if disk.major() == 0 {
        return Err(DriverError::InvalidInput);
    }
    let device_number = disk.device_number();
    let mut devices = BLOCK_DEVICES.lock();
    let first_minor = disk.first_minor();
    let last_minor = first_minor + disk.minors() - 1;
    let conflicts = devices.values().any(|registered| {
        if registered.major() != disk.major() {
            return false;
        }
        let registered_first = registered.first_minor();
        let registered_last = registered_first + registered.minors() - 1;
        first_minor <= registered_last && registered_first <= last_minor
    });
    if conflicts || devices.contains_key(&device_number) {
        return Err(DriverError::AlreadyExists);
    }
    devices.insert(device_number, disk.clone());
    Ok(disk.part0())
}

/// Removes a generic disk and its block-device views.
pub fn del_gendisk(device_number: DeviceNumber) -> Option<GendiskRef> {
    BLOCK_DEVICES.lock().remove(&device_number)
}

/// Looks up a registered block-device view by `dev_t`.
pub fn lookup_block_device(device_number: DeviceNumber) -> Option<Arc<BlockDevice>> {
    BLOCK_DEVICES
        .lock()
        .get(&device_number)
        .map(|disk| disk.part0())
}

/// Returns a snapshot of all registered block-device views.
pub fn block_devices() -> Vec<Arc<BlockDevice>> {
    BLOCK_DEVICES
        .lock()
        .values()
        .map(|disk| disk.part0())
        .collect()
}

#[unittest::mod_test]
mod tests {
    use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};

    use ksync::Mutex;
    use unittest::def_test;

    use super::*;

    const BLOCK_SIZE: usize = 512;

    struct MemoryDisk {
        block_size: usize,
        storage: Mutex<Vec<u8>>,
    }

    struct GeometryDisk {
        blocks: u64,
        block_size: usize,
    }

    struct InherentlyReadOnlyDisk(MemoryDisk);

    impl MemoryDisk {
        fn new(blocks: u64) -> Self {
            Self::with_block_size(blocks, BLOCK_SIZE)
        }

        fn with_block_size(blocks: u64, block_size: usize) -> Self {
            let blocks = usize::try_from(blocks).expect("test block count must fit in usize");
            let len = blocks
                .checked_mul(block_size)
                .expect("test disk size must fit in usize");
            Self {
                block_size,
                storage: Mutex::new(vec![0; len]),
            }
        }
    }

    impl BlockDeviceOperations for MemoryDisk {
        fn num_blocks(&self) -> u64 {
            (self.storage.lock().len() / self.block_size) as u64
        }

        fn block_size(&self) -> usize {
            self.block_size
        }

        fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
            let block_id = usize::try_from(block_id).map_err(|_| DriverError::InvalidInput)?;
            let start = block_id
                .checked_mul(self.block_size)
                .ok_or(DriverError::InvalidInput)?;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::InvalidInput)?;
            let storage = self.storage.lock();
            let source = storage.get(start..end).ok_or(DriverError::InvalidInput)?;
            buf.copy_from_slice(source);
            Ok(())
        }

        fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
            let block_id = usize::try_from(block_id).map_err(|_| DriverError::InvalidInput)?;
            let start = block_id
                .checked_mul(self.block_size)
                .ok_or(DriverError::InvalidInput)?;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::InvalidInput)?;
            let mut storage = self.storage.lock();
            let target = storage
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?;
            target.copy_from_slice(buf);
            Ok(())
        }

        fn flush(&self) -> DriverResult {
            Ok(())
        }
    }

    impl BlockDeviceOperations for GeometryDisk {
        fn num_blocks(&self) -> u64 {
            self.blocks
        }

        fn block_size(&self) -> usize {
            self.block_size
        }

        fn read_block(&self, _block_id: u64, _buf: &mut [u8]) -> DriverResult {
            Err(DriverError::Unsupported)
        }

        fn write_block(&self, _block_id: u64, _buf: &[u8]) -> DriverResult {
            Err(DriverError::Unsupported)
        }

        fn flush(&self) -> DriverResult {
            Ok(())
        }
    }

    impl BlockDeviceOperations for InherentlyReadOnlyDisk {
        fn num_blocks(&self) -> u64 {
            self.0.num_blocks()
        }

        fn block_size(&self) -> usize {
            self.0.block_size()
        }

        fn is_inherently_read_only(&self) -> bool {
            true
        }

        fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
            self.0.read_block(block_id, buf)
        }

        fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
            self.0.write_block(block_id, buf)
        }

        fn flush(&self) -> DriverResult {
            self.0.flush()
        }
    }

    fn disk(name: &str, major: u32, first_minor: u32, minors: u32) -> GendiskRef {
        Gendisk::alloc(
            String::from(name),
            major,
            first_minor,
            minors,
            Box::new(MemoryDisk::new(8)),
        )
        .expect("valid test disk")
    }

    fn ioctl_bytes(device: &BlockDevice, cmd: u32) -> Vec<u8> {
        let mut output = Vec::new();
        device
            .ioctl(
                BlockOpenMode::READ,
                cmd,
                1,
                false,
                || panic!("query must not read the argument"),
                |bytes| {
                    output.extend_from_slice(bytes);
                    Ok(())
                },
            )
            .expect("generic query succeeds without driver ioctl support");
        output
    }

    #[def_test]
    fn generic_ioctls_report_queue_geometry_and_sector_capacity() {
        use linux_raw_sys::ioctl::{
            BLKALIGNOFF, BLKGETDISKSEQ, BLKGETSIZE, BLKGETSIZE64, BLKIOMIN, BLKIOOPT, BLKPBSZGET,
            BLKROGET, BLKSSZGET,
        };

        let disk = Gendisk::alloc(
            String::from("ioctl-4k"),
            249,
            0,
            1,
            Box::new(GeometryDisk {
                blocks: 2,
                block_size: 4096,
            }),
        )
        .unwrap();
        let device = disk.part0();
        for (cmd, expected) in [
            (BLKSSZGET, 4096_u32),
            (BLKPBSZGET, 4096),
            (BLKIOMIN, 4096),
            (BLKIOOPT, 0),
            (BLKALIGNOFF, 0),
            (BLKROGET, 0),
        ] {
            assert_eq!(ioctl_bytes(&device, cmd), expected.to_ne_bytes());
        }
        device.set_capacity(9).unwrap();
        assert_eq!(ioctl_bytes(&device, BLKGETSIZE), 9_usize.to_ne_bytes());
        assert_eq!(ioctl_bytes(&device, BLKGETSIZE64), 4608_u64.to_ne_bytes());
        assert_eq!(
            ioctl_bytes(&device, BLKGETDISKSEQ),
            disk.diskseq().to_ne_bytes()
        );
    }

    #[def_test]
    fn readahead_ioctls_share_disk_policy_and_linux_argument_semantics() {
        use linux_raw_sys::ioctl::{BLKFRAGET, BLKFRASET, BLKRAGET, BLKRASET};

        let disk = disk("ioctl-readahead", 249, 0, 1);
        let device = disk.part0();
        let set = |cmd, sectors, has_sys_admin| {
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                cmd,
                sectors,
                has_sys_admin,
                || panic!("readahead setter takes a value, not a pointer"),
                |_| panic!("setter has no output"),
            )
        };
        for cmd in [BLKRAGET, BLKFRAGET] {
            assert_eq!(ioctl_bytes(&device, cmd), 256_usize.to_ne_bytes());
            assert_eq!(
                device.as_ref().ioctl(
                    BlockOpenMode::READ,
                    cmd,
                    0,
                    false,
                    || panic!("getter has no input"),
                    |_| panic!("null is rejected before copying"),
                ),
                Err(KError::InvalidInput)
            );
            assert_eq!(
                device.as_ref().ioctl(
                    BlockOpenMode::READ,
                    cmd,
                    1,
                    false,
                    || panic!("getter has no input"),
                    |_| Err(KError::BadAddress),
                ),
                Err(KError::BadAddress)
            );
        }
        for cmd in [BLKRASET, BLKFRASET] {
            assert_eq!(set(cmd, 9, false), Err(KError::PermissionDenied));
            assert_eq!(set(cmd, 9, true), Ok(0));
            assert_eq!(disk.backing_dev_info().readahead_pages(), 1);
            for getter in [BLKRAGET, BLKFRAGET] {
                assert_eq!(ioctl_bytes(&device, getter), 8_usize.to_ne_bytes());
            }
            assert_eq!(set(cmd, 0, true), Ok(0));
            assert_eq!(disk.backing_dev_info().readahead_pages(), 0);
        }
    }

    #[def_test(serial)]
    fn disk_sequence_tracks_media_not_publication_or_capacity() {
        let first = disk("sequence-first", 249, 0, 1);
        let sequence = first.diskseq();
        assert_ne!(sequence, 0);
        let device = add_disk(first.clone()).unwrap();
        assert_eq!(device.disk().diskseq(), sequence);
        device.set_capacity(3).unwrap();
        del_gendisk(first.device_number()).unwrap();
        add_disk(first.clone()).unwrap();
        assert_eq!(first.diskseq(), sequence);
        del_gendisk(first.device_number()).unwrap();
        let second = disk("sequence-reused-devt", 249, 0, 1);
        assert!(second.diskseq() > sequence);
        first.inc_diskseq();
        assert!(device.disk().diskseq() > second.diskseq());
        assert_eq!(device.num_sectors(), 3);
    }

    #[def_test]
    fn unpublished_part0_keeps_its_complete_disk_alive() {
        let backend = Arc::new(MemoryDisk::new(8));
        let backend_weak = Arc::downgrade(&backend);
        let disk = Gendisk::alloc(
            String::from("test-unpublished"),
            239,
            0,
            1,
            Box::new(backend),
        )
        .expect("valid unpublished disk");
        let part0 = disk.part0();
        let disk_weak = Arc::downgrade(&part0.disk);
        let part0_weak = Arc::downgrade(&part0);
        let queue_weak = Arc::downgrade(disk.queue());
        let disk_identity = part0.disk() as *const Gendisk;

        assert_eq!(part0.device_number(), DeviceNumber::new(239, 0));
        assert_eq!(part0.start_sector(), 0);
        assert_eq!(part0.num_sectors(), 8);
        assert!(Arc::ptr_eq(disk.queue(), part0.queue()));
        assert!(core::ptr::eq(part0.disk(), &*disk));
        assert!(lookup_block_device(part0.device_number()).is_none());
        drop(disk);

        assert!(core::ptr::eq(part0.disk(), disk_identity));
        assert_eq!(part0.disk().name(), "test-unpublished");
        assert!(Arc::ptr_eq(&part0.disk().part0(), &part0));
        part0
            .write_block(0, &[0x5a; BLOCK_SIZE])
            .expect("write unpublished disk");
        let mut data = [0; BLOCK_SIZE];
        part0
            .read_block(0, &mut data)
            .expect("read unpublished disk");
        assert_eq!(data, [0x5a; BLOCK_SIZE]);
        assert!(backend_weak.upgrade().is_some());
        drop(part0);

        assert!(part0_weak.upgrade().is_none());
        assert!(disk_weak.upgrade().is_none());
        assert!(queue_weak.upgrade().is_none());
        assert!(backend_weak.upgrade().is_none());
    }

    #[def_test]
    fn cloned_disk_reference_keeps_canonical_part0_alive() {
        let disk = disk("test-disk-reference", 249, 0, 1);
        let part0 = disk.part0();
        let part0_weak = Arc::downgrade(&part0);
        let disk_weak = Arc::downgrade(&part0.disk);
        let retained = disk.clone();
        drop(part0);
        drop(disk);

        assert!(Arc::ptr_eq(
            &retained.part0(),
            &part0_weak.upgrade().expect("retained part0"),
        ));
        assert!(core::ptr::eq(retained.part0().disk(), &*retained));
        drop(retained);
        assert!(part0_weak.upgrade().is_none());
        assert!(disk_weak.upgrade().is_none());
    }

    #[def_test(serial)]
    fn add_disk_publishes_canonical_part0_and_checks_minor_ranges() {
        let first = disk("test-range-a", 240, 64, 16);
        let part0 = add_disk(first.clone()).expect("publish first disk");
        assert!(Arc::ptr_eq(
            &part0,
            &lookup_block_device(DeviceNumber::new(240, 64)).expect("lookup part0")
        ));
        assert!(Arc::ptr_eq(&part0, &first.part0()));

        let overlap = disk("test-range-overlap", 240, 70, 1);
        assert!(matches!(add_disk(overlap), Err(DriverError::AlreadyExists)));

        let adjacent = disk("test-range-b", 240, 80, 1);
        add_disk(adjacent.clone()).expect("adjacent range is valid");
        del_gendisk(adjacent.device_number()).expect("remove adjacent disk");
        del_gendisk(first.device_number()).expect("remove first disk");
    }

    #[def_test(serial)]
    fn block_device_validates_the_complete_io_extent() {
        let disk = disk("test-bounds", 241, 0, 1);
        let device = add_disk(disk.clone()).expect("publish bounds disk");
        let mut two_blocks = vec![0; BLOCK_SIZE * 2];
        assert_eq!(
            device.read_block(7, &mut two_blocks).unwrap_err(),
            DriverError::InvalidInput
        );
        assert_eq!(
            device.write_block(0, &[0; BLOCK_SIZE - 1]).unwrap_err(),
            DriverError::InvalidInput
        );
        assert!(device.read_block(8, &mut []).is_ok());
        del_gendisk(disk.device_number()).expect("remove bounds disk");
    }

    #[def_test(serial)]
    fn set_capacity_updates_the_resident_block_device() {
        let disk = disk("test-capacity", 242, 0, 1);
        let device = add_disk(disk.clone()).expect("publish capacity disk");
        assert_eq!(device.num_sectors(), 8);
        device.set_capacity(3).expect("valid capacity");
        assert_eq!(device.num_sectors(), 3);
        assert_eq!(device.size(), 3 * BLOCK_SIZE as u64);
        del_gendisk(disk.device_number()).expect("remove capacity disk");
    }

    #[def_test]
    fn gendisk_rejects_invalid_capacity_geometry() {
        let zero_block_size = Gendisk::alloc(
            String::from("zero-block-size"),
            245,
            0,
            1,
            Box::new(GeometryDisk {
                blocks: 1,
                block_size: 0,
            }),
        );
        assert!(matches!(zero_block_size, Err(DriverError::InvalidInput)));

        let overflowing_size = Gendisk::alloc(
            String::from("overflowing-size"),
            245,
            0,
            1,
            Box::new(GeometryDisk {
                blocks: u64::MAX,
                block_size: BLOCK_SIZE,
            }),
        );
        assert!(matches!(overflowing_size, Err(DriverError::InvalidInput)));

        let oversized_block = Gendisk::alloc(
            String::from("oversized-logical-block"),
            245,
            0,
            1,
            Box::new(GeometryDisk {
                blocks: 1,
                block_size: 8192,
            }),
        );
        assert!(matches!(oversized_block, Err(DriverError::InvalidInput)));
    }

    #[def_test]
    fn four_kib_logical_io_uses_sector_capacity() {
        let disk = Gendisk::alloc(
            String::from("four-k-logical-blocks"),
            247,
            0,
            1,
            Box::new(MemoryDisk::with_block_size(2, 4096)),
        )
        .expect("valid 4-KiB logical geometry");

        let limits = disk.queue().limits();
        assert_eq!(limits.logical_block_size(), 4096);
        assert_eq!(limits.physical_block_size(), 4096);
        assert_eq!(limits.io_min(), 4096);
        assert_eq!(limits.io_opt(), 0);
        assert_eq!(limits.alignment_offset(), 0);
        assert_eq!(disk.part0().num_sectors(), 16);
        assert_eq!(disk.part0().size(), 8192);

        let written = vec![0x5a; 4096];
        disk.part0()
            .write_block(1, &written)
            .expect("write second logical block");
        let mut read = vec![0; 4096];
        disk.part0()
            .read_block(1, &mut read)
            .expect("read second logical block");
        assert_eq!(read, written);
        assert_eq!(
            disk.part0().read_block(2, &mut read).unwrap_err(),
            DriverError::InvalidInput
        );

        disk.part0()
            .set_capacity(9)
            .expect("sector capacity need not fill a logical block");
        assert_eq!(disk.part0().size(), 9 * u64::from(SECTOR_SIZE));
        assert_eq!(
            BlockDeviceOperations::read_block(disk.part0().as_ref(), 1, &mut read).unwrap_err(),
            DriverError::InvalidInput
        );
    }

    #[def_test(serial)]
    fn existing_part0_outlives_unpublication() {
        let disk = disk("test-unpublished-lifetime", 248, 0, 1);
        let device = add_disk(disk.clone()).expect("publish lifetime test disk");
        let disk_weak = Arc::downgrade(&device.disk);
        let part0_weak = Arc::downgrade(&device);
        let queue_weak = Arc::downgrade(device.queue());
        let disk_identity = device.disk() as *const Gendisk;
        let removed = del_gendisk(disk.device_number()).expect("unpublish lifetime test disk");
        assert!(core::ptr::eq(&*removed, &*disk));
        drop(removed);
        drop(disk);

        assert!(lookup_block_device(device.device_number()).is_none());
        assert!(core::ptr::eq(device.disk(), disk_identity));
        assert!(Arc::ptr_eq(&device.disk().part0(), &device));
        assert!(Arc::ptr_eq(device.disk().queue(), device.queue()));
        let mut data = vec![0; BLOCK_SIZE];
        device
            .read_block(0, &mut data)
            .expect("existing part0 keeps its disk alive");
        device
            .set_disk_read_only(true)
            .expect("disk state is still available");
        assert_eq!(device.write_block(0, &data), Err(DriverError::ReadOnly));
        drop(device);
        assert!(part0_weak.upgrade().is_none());
        assert!(disk_weak.upgrade().is_none());
        assert!(queue_weak.upgrade().is_none());
    }

    #[def_test(serial)]
    fn read_only_state_is_owned_by_the_block_core() {
        let disk = disk("test-read-only", 244, 0, 1);
        let device = add_disk(disk.clone()).expect("publish read-only disk");
        use linux_raw_sys::ioctl::BLKROSET;

        assert_eq!(
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                BLKROSET,
                0,
                false,
                || panic!("authorization must precede argument access"),
                |_| panic!("setter must not copy out"),
            ),
            Err(KError::PermissionDenied)
        );
        assert!(!device.is_read_only());
        assert_eq!(
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                BLKROSET,
                0,
                true,
                || Err(KError::BadAddress),
                |_| panic!("setter must not copy out"),
            ),
            Err(KError::BadAddress)
        );
        assert!(!device.is_read_only());
        for value in [-1, 2] {
            device
                .as_ref()
                .ioctl(
                    BlockOpenMode::READ,
                    BLKROSET,
                    0,
                    true,
                    || Ok(value),
                    |_| panic!("setter must not copy out"),
                )
                .expect("nonzero int sets disk read-only");
            assert!(device.is_read_only());
        }
        assert!(device.is_read_only());
        assert_eq!(
            device.write_block(0, &[0; BLOCK_SIZE]).unwrap_err(),
            DriverError::ReadOnly
        );
        assert_eq!(
            BlockDeviceOperations::write_block(device.as_ref(), 0, &[0; BLOCK_SIZE]).unwrap_err(),
            DriverError::ReadOnly
        );
        device
            .as_ref()
            .ioctl(
                BlockOpenMode::READ,
                BLKROSET,
                0,
                true,
                || Ok(0),
                |_| panic!("setter must not copy out"),
            )
            .expect("zero clears administrative read-only state");
        assert!(device.write_block(0, &[0; BLOCK_SIZE]).is_ok());
        del_gendisk(disk.device_number()).expect("remove read-only disk");
    }

    #[def_test(serial)]
    fn inherently_read_only_disk_cannot_be_made_writable() {
        let disk = Gendisk::alloc(
            String::from("test-inherently-read-only"),
            246,
            0,
            1,
            Box::new(InherentlyReadOnlyDisk(MemoryDisk::new(8))),
        )
        .expect("valid inherently read-only disk");
        let device = add_disk(disk.clone()).expect("publish inherently read-only disk");

        assert!(device.is_read_only());
        assert!(matches!(
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                linux_raw_sys::ioctl::BLKROSET,
                0,
                true,
                || Ok(0),
                |_| panic!("setter must not copy out"),
            ),
            Ok(0)
        ));
        assert!(device.is_read_only());
        assert_eq!(
            device.write_block(0, &[0; BLOCK_SIZE]).unwrap_err(),
            DriverError::ReadOnly
        );

        del_gendisk(disk.device_number()).expect("remove inherently read-only disk");
    }

    #[def_test(serial)]
    fn exclusive_claim_follows_token_lifetime() {
        let disk = disk("test-exclusive-claim", 243, 0, 1);
        let device = add_disk(disk.clone()).expect("publish claim test disk");

        let first = device.claim_exclusive().expect("claim unowned device");
        assert!(matches!(
            device.claim_exclusive(),
            Err(KError::ResourceBusy)
        ));

        first.release();
        let second = device
            .claim_exclusive()
            .expect("explicit release makes device claimable");
        drop(second);
        device
            .claim_exclusive()
            .expect("drop makes device claimable");

        del_gendisk(disk.device_number()).expect("remove claim test disk");
    }
}
