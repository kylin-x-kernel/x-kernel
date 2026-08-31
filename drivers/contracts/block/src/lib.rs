// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-style block-device core.
//!
//! Drivers provide [`BlockDeviceOperations`]. The block core owns persistent
//! disk identity in [`Gendisk`], publishes the whole-disk [`BlockDevice`] view,
//! validates capacity and I/O extents, and keeps generic disk state such as
//! read-only status out of individual filesystems and drivers.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

// #[cfg(feature = "ahci")]
// pub mod ahci;
// #[cfg(feature = "sdmmc")]
// pub mod sdmmc;

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

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
    fn ioctl(
        &self,
        _device: &BlockDevice,
        _mode: BlockOpenMode,
        _cmd: u32,
        _arg: usize,
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
    ) -> KResult<usize> {
        (**self).ioctl(device, mode, cmd, arg)
    }

    fn set_read_only(&self, device: &BlockDevice, read_only: bool) -> KResult<()> {
        (**self).set_read_only(device, read_only)
    }
}

/// A registered generic disk.
///
/// This is the object-oriented counterpart of Linux `struct gendisk`: it owns
/// the user-visible disk identity and delegates I/O to driver-private
/// operations.
pub struct Gendisk {
    name: String,
    major: u32,
    first_minor: u32,
    minors: u32,
    is_inherently_read_only: bool,
    state: AtomicUsize,
    operations: Box<dyn BlockDeviceOperations + Send + Sync>,
}

impl Gendisk {
    /// Creates a disk from driver-private I/O operations.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] for an empty name, an invalid
    /// minor range, a zero block size, or a byte capacity that overflows `u64`.
    pub fn new(
        name: String,
        major: u32,
        first_minor: u32,
        minors: u32,
        operations: Box<dyn BlockDeviceOperations + Send + Sync>,
    ) -> DriverResult<Self> {
        let block_size = operations.block_size();
        let capacity_is_valid = u64::try_from(block_size)
            .ok()
            .filter(|block_size| *block_size != 0)
            .and_then(|block_size| operations.num_blocks().checked_mul(block_size))
            .is_some();
        if name.is_empty()
            || minors == 0
            || first_minor.checked_add(minors - 1).is_none()
            || !capacity_is_valid
        {
            return Err(DriverError::InvalidInput);
        }
        let is_inherently_read_only = operations.is_inherently_read_only();
        Ok(Self {
            name,
            major,
            first_minor,
            minors,
            is_inherently_read_only,
            state: AtomicUsize::new(0),
            operations,
        })
    }

    /// Returns the block major assigned to this disk.
    pub const fn major(&self) -> u32 {
        self.major
    }

    /// Returns the first minor assigned to this disk.
    pub const fn first_minor(&self) -> u32 {
        self.first_minor
    }

    /// Returns the number of consecutive minors owned by this disk.
    pub const fn minors(&self) -> u32 {
        self.minors
    }

    /// Returns the user-visible disk name (`disk_name`).
    pub fn name(&self) -> &str {
        &self.name
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

impl BlockDeviceOperations for Gendisk {
    fn num_blocks(&self) -> u64 {
        self.operations.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.operations.block_size()
    }

    fn is_inherently_read_only(&self) -> bool {
        self.is_inherently_read_only
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.operations.read_block(block_id, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        self.operations.write_block(block_id, buf)
    }

    fn flush(&self) -> DriverResult {
        self.operations.flush()
    }

    fn open(&self, device: &BlockDevice, mode: BlockOpenMode) -> KResult<()> {
        self.operations.open(device, mode)
    }

    fn release(&self, device: &BlockDevice) {
        self.operations.release(device)
    }

    fn ioctl(
        &self,
        device: &BlockDevice,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
    ) -> KResult<usize> {
        self.operations.ioctl(device, mode, cmd, arg)
    }

    fn set_read_only(&self, device: &BlockDevice, read_only: bool) -> KResult<()> {
        self.operations.set_read_only(device, read_only)
    }
}

/// A block-device view identified by one Linux-style device number.
///
/// This corresponds to Linux `struct block_device`. The current block core
/// creates the whole-disk view (`part0`); the start and length fields already
/// express the Linux partition boundary without introducing a second device
/// identity model.
pub struct BlockDevice {
    disk: Arc<Gendisk>,
    device_number: DeviceNumber,
    start_block: u64,
    num_blocks: AtomicU64,
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
    fn whole(disk: Arc<Gendisk>) -> Self {
        let device_number = disk.device_number();
        let num_blocks = disk.num_blocks();
        Self {
            disk,
            device_number,
            start_block: 0,
            num_blocks: AtomicU64::new(num_blocks),
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

    /// Returns the owning generic disk (`bd_disk`).
    pub fn disk(&self) -> &Arc<Gendisk> {
        &self.disk
    }

    /// Returns the first backend block visible through this device.
    pub const fn start_block(&self) -> u64 {
        self.start_block
    }

    /// Returns the number of backend blocks visible through this device.
    pub fn num_blocks(&self) -> u64 {
        self.num_blocks.load(Ordering::Acquire)
    }

    /// Updates this view's capacity in backend blocks.
    ///
    /// This is the object-oriented equivalent of Linux `set_capacity()` and is
    /// used by media whose size changes after publication, such as loop disks.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] when the resulting byte capacity
    /// cannot be represented in `u64`.
    pub fn set_capacity(&self, num_blocks: u64) -> DriverResult<()> {
        let block_size = u64::try_from(self.block_size()).map_err(|_| DriverError::InvalidInput)?;
        num_blocks
            .checked_mul(block_size)
            .ok_or(DriverError::InvalidInput)?;
        self.num_blocks.store(num_blocks, Ordering::Release);
        Ok(())
    }

    /// Returns the size of one backend block in bytes.
    pub fn block_size(&self) -> usize {
        self.disk.block_size()
    }

    /// Returns this device's capacity in bytes.
    pub fn size(&self) -> u64 {
        let block_size = u64::try_from(self.block_size()).expect("validated block size");
        self.num_blocks()
            .checked_mul(block_size)
            .expect("validated block-device capacity")
    }

    /// Returns whether inherent or administrative policy prohibits writes.
    pub fn is_read_only(&self) -> bool {
        self.disk.is_inherently_read_only
            || self.disk.state.load(Ordering::Acquire) & DISK_STATE_ADMIN_READ_ONLY != 0
    }

    /// Changes this disk's canonical read-only state.
    ///
    /// The driver callback runs before the generic state is published, matching
    /// Linux `BLKROSET` ordering.
    ///
    /// # Errors
    ///
    /// Returns [`KError::ReadOnlyFilesystem`] when attempting to make an
    /// inherently read-only disk writable. Returns a driver callback error
    /// without changing the administrative state.
    pub fn set_disk_read_only(&self, is_read_only: bool) -> KResult<()> {
        if !is_read_only && self.disk.is_inherently_read_only {
            return Err(KError::ReadOnlyFilesystem);
        }
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
        self.disk.name()
    }

    fn validate_io(&self, block_id: u64, len: usize) -> DriverResult<()> {
        let block_size = self.block_size();
        if block_size == 0 || !len.is_multiple_of(block_size) {
            return Err(DriverError::InvalidInput);
        }
        let blocks = u64::try_from(len / block_size).map_err(|_| DriverError::InvalidInput)?;
        let end = block_id
            .checked_add(blocks)
            .ok_or(DriverError::InvalidInput)?;
        let capacity = self.num_blocks();
        if block_id > capacity || end > capacity {
            return Err(DriverError::InvalidInput);
        }
        Ok(())
    }
}

impl Device for BlockDevice {
    fn name(&self) -> &str {
        self.disk.name()
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn irq(&self) -> Option<usize> {
        self.disk.irq()
    }
}

impl BlockDeviceOperations for BlockDevice {
    fn num_blocks(&self) -> u64 {
        self.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.disk.block_size()
    }

    fn is_inherently_read_only(&self) -> bool {
        self.disk.is_inherently_read_only
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.validate_io(block_id, buf.len())?;
        let backend_block = self
            .start_block
            .checked_add(block_id)
            .ok_or(DriverError::InvalidInput)?;
        self.disk.read_block(backend_block, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        if self.is_read_only() {
            return Err(DriverError::ReadOnly);
        }
        self.validate_io(block_id, buf.len())?;
        let backend_block = self
            .start_block
            .checked_add(block_id)
            .ok_or(DriverError::InvalidInput)?;
        self.disk.write_block(backend_block, buf)
    }

    fn flush(&self) -> DriverResult {
        self.disk.flush()
    }

    fn open(&self, _device: &BlockDevice, mode: BlockOpenMode) -> KResult<()> {
        self.disk.open(self, mode)
    }

    fn release(&self, _device: &BlockDevice) {
        self.disk.release(self);
    }

    fn ioctl(
        &self,
        _device: &BlockDevice,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
    ) -> KResult<usize> {
        self.disk.ioctl(self, mode, cmd, arg)
    }

    fn set_read_only(&self, _device: &BlockDevice, read_only: bool) -> KResult<()> {
        self.disk.set_read_only(self, read_only)
    }
}

impl Gendisk {
    /// Returns the device number assigned to the whole disk.
    pub const fn device_number(&self) -> DeviceNumber {
        DeviceNumber::new(self.major, self.first_minor)
    }

    /// Returns this disk's registered whole-device view, if it is added.
    pub fn part0(&self) -> Option<Arc<BlockDevice>> {
        lookup_block_device(self.device_number())
            .filter(|device| core::ptr::eq::<Gendisk>(device.disk().as_ref(), self))
    }
}

static BLOCK_DEVICES: Lazy<Mutex<BTreeMap<DeviceNumber, Arc<BlockDevice>>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

/// Adds a generic disk and creates its whole-device `part0` view.
///
/// # Errors
///
/// Returns [`DriverError::InvalidInput`] when no major has been assigned, or
/// [`DriverError::AlreadyExists`] when the device-number range conflicts with
/// an existing disk.
pub fn add_disk(disk: Arc<Gendisk>) -> DriverResult<Arc<BlockDevice>> {
    if disk.major() == 0 {
        return Err(DriverError::InvalidInput);
    }
    let device_number = disk.device_number();
    let part0 = Arc::new(BlockDevice::whole(disk.clone()));
    let mut devices = BLOCK_DEVICES.lock();
    let first_minor = disk.first_minor();
    let last_minor = first_minor + disk.minors() - 1;
    let conflicts = devices.values().any(|device| {
        let registered = device.disk();
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
    devices.insert(device_number, part0.clone());
    Ok(part0)
}

/// Removes a generic disk and its block-device views.
pub fn del_gendisk(device_number: DeviceNumber) -> Option<Arc<Gendisk>> {
    let mut devices = BLOCK_DEVICES.lock();
    let disk = devices.get(&device_number)?.disk().clone();
    devices.retain(|_, device| !Arc::ptr_eq(device.disk(), &disk));
    Some(disk)
}

/// Looks up a registered block-device view by `dev_t`.
pub fn lookup_block_device(device_number: DeviceNumber) -> Option<Arc<BlockDevice>> {
    BLOCK_DEVICES.lock().get(&device_number).cloned()
}

/// Returns a snapshot of all registered block-device views.
pub fn block_devices() -> Vec<Arc<BlockDevice>> {
    BLOCK_DEVICES.lock().values().cloned().collect()
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};

    use ksync::Mutex;
    use unittest::def_test;

    use super::*;

    const BLOCK_SIZE: usize = 512;

    struct MemoryDisk(Mutex<Vec<u8>>);

    struct GeometryDisk {
        blocks: u64,
        block_size: usize,
    }

    struct InherentlyReadOnlyDisk(MemoryDisk);

    impl MemoryDisk {
        fn new(blocks: u64) -> Self {
            Self(Mutex::new(vec![0; blocks as usize * BLOCK_SIZE]))
        }
    }

    impl BlockDeviceOperations for MemoryDisk {
        fn num_blocks(&self) -> u64 {
            (self.0.lock().len() / BLOCK_SIZE) as u64
        }

        fn block_size(&self) -> usize {
            BLOCK_SIZE
        }

        fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
            let start = block_id as usize * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::InvalidInput)?;
            let storage = self.0.lock();
            let source = storage.get(start..end).ok_or(DriverError::InvalidInput)?;
            buf.copy_from_slice(source);
            Ok(())
        }

        fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
            let start = block_id as usize * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::InvalidInput)?;
            let mut storage = self.0.lock();
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

    fn disk(name: &str, major: u32, first_minor: u32, minors: u32) -> Arc<Gendisk> {
        Arc::new(
            Gendisk::new(
                String::from(name),
                major,
                first_minor,
                minors,
                Box::new(MemoryDisk::new(8)),
            )
            .expect("valid test disk"),
        )
    }

    #[def_test(serial)]
    fn add_disk_publishes_canonical_part0_and_checks_minor_ranges() {
        let first = disk("test-range-a", 240, 64, 16);
        let part0 = add_disk(first.clone()).expect("publish first disk");
        assert!(Arc::ptr_eq(
            &part0,
            &lookup_block_device(DeviceNumber::new(240, 64)).expect("lookup part0")
        ));
        assert!(Arc::ptr_eq(&part0, &first.part0().expect("gendisk part0")));

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
        assert_eq!(device.num_blocks(), 8);
        device.set_capacity(3).expect("valid capacity");
        assert_eq!(device.num_blocks(), 3);
        assert_eq!(device.size(), 3 * BLOCK_SIZE as u64);
        del_gendisk(disk.device_number()).expect("remove capacity disk");
    }

    #[def_test]
    fn gendisk_rejects_invalid_capacity_geometry() {
        let zero_block_size = Gendisk::new(
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

        let overflowing_size = Gendisk::new(
            String::from("overflowing-size"),
            245,
            0,
            1,
            Box::new(GeometryDisk {
                blocks: u64::MAX,
                block_size: 2,
            }),
        );
        assert!(matches!(overflowing_size, Err(DriverError::InvalidInput)));
    }

    #[def_test(serial)]
    fn read_only_state_is_owned_by_the_block_core() {
        let disk = disk("test-read-only", 244, 0, 1);
        let device = add_disk(disk.clone()).expect("publish read-only disk");
        device.set_disk_read_only(true).expect("set disk read-only");
        assert!(device.is_read_only());
        assert_eq!(
            device.write_block(0, &[0; BLOCK_SIZE]).unwrap_err(),
            DriverError::ReadOnly
        );
        device.set_disk_read_only(false).expect("set disk writable");
        assert!(device.write_block(0, &[0; BLOCK_SIZE]).is_ok());
        del_gendisk(disk.device_number()).expect("remove read-only disk");
    }

    #[def_test(serial)]
    fn inherently_read_only_disk_cannot_be_made_writable() {
        let disk = Arc::new(
            Gendisk::new(
                String::from("test-inherently-read-only"),
                246,
                0,
                1,
                Box::new(InherentlyReadOnlyDisk(MemoryDisk::new(8))),
            )
            .expect("valid inherently read-only disk"),
        );
        let device = add_disk(disk.clone()).expect("publish inherently read-only disk");

        assert!(device.is_read_only());
        assert!(matches!(
            device.set_disk_read_only(false),
            Err(KError::ReadOnlyFilesystem)
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
