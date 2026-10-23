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
mod bio;
pub mod completion;
mod ioctl;
pub mod word_io;

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
pub use bio::{Bio, BioMemory, BioOperation, BioOperations, BioVec, BioVecRef};
use kdevice::DeviceNumber;
use kerrno::{KError, KResult};
use klazy::Lazy;
use ksync::Mutex;

// Compile the actual adapter in kernel UT without enabling the legacy SDK or
// probing hardware. Only the controller import is replaced by a test fixture.
// Production re-enablement must extend this one predicate, not add a module.
#[cfg(unittest)]
pub mod bcm2835sdhci;
unittest::path_mod_test!(mod bcm_controller_fixture, "../ut-tests/bcm_controller_fixture.rs");

// Keep the production SDK disabled; kernel UT compiles this same adapter with
// a CPU controller to check that non-coherent HALs never enter the SDK.
#[cfg(unittest)]
pub mod ahci;
unittest::path_mod_test!(mod ahci_controller_fixture, "../ut-tests/ahci_controller_fixture.rs");

#[cfg(feature = "ramdisk")]
pub mod ramdisk;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_static;

#[cfg(feature = "ramdisk-static")]
pub mod ramdisk_image;

mod queue;

// #[cfg(feature = "sdmmc")]
// pub mod sdmmc;

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use queue::{BlkMqOperations, QueueLimits, Request, RequestQueue, SECTOR_SHIFT, SECTOR_SIZE};

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

/// Disk geometry and control operations; data dispatch belongs to the queue.
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
    operations: Arc<dyn BlockDeviceOperations>,
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
        operations: Box<dyn BioOperations>,
    ) -> DriverResult<GendiskRef> {
        let operations: Arc<dyn BioOperations> = Arc::from(operations);
        Self::alloc_with_queue(
            name,
            major,
            first_minor,
            minors,
            operations.clone(),
            queue::Dispatch::Bio(operations),
        )
    }

    /// Creates a request-based disk sharing one backend between control and I/O.
    ///
    /// # Errors
    /// Returns the same identity, geometry and capacity errors as `alloc`.
    pub fn alloc_mq<T: BlockDeviceOperations + BlkMqOperations + 'static>(
        name: String,
        major: u32,
        first_minor: u32,
        minors: u32,
        operations: Arc<T>,
    ) -> DriverResult<GendiskRef> {
        Self::alloc_with_queue(
            name,
            major,
            first_minor,
            minors,
            operations.clone(),
            queue::Dispatch::Request(operations),
        )
    }

    fn alloc_with_queue(
        name: String,
        major: u32,
        first_minor: u32,
        minors: u32,
        operations: Arc<dyn BlockDeviceOperations>,
        dispatch: queue::Dispatch,
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
        let queue = Arc::new(RequestQueue::new(limits, dispatch));
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
    /// Accepts any buffer start address and reads directly into the borrowed range.
    /// The call cannot return until completion has retired all device access.
    /// An error may leave the destination partially modified.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::InvalidInput`] for a non-block-sized or out-of-range
    /// request. Backend failures are propagated unchanged. Unwinding builds return
    /// Unsupported before lending memory.
    pub fn read_block(self: &Arc<Self>, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let sector = block_id
            .checked_mul(self.queue.limits().logical_block_sectors())
            .ok_or(DriverError::InvalidInput)?;
        if buf.is_empty() {
            return if sector <= self.num_sectors() {
                Ok(())
            } else {
                Err(DriverError::InvalidInput)
            };
        }
        // No unwind may bypass the mandatory completion wait after publication.
        if cfg!(panic = "unwind") {
            return Err(DriverError::Unsupported);
        }
        // SAFETY: this private vector never escapes to a caller-owned handle.
        // submit_bio_wait waits non-interruptibly and drops returned vectors
        // before this exclusive buffer borrow can end, including on I/O error.
        let vector = unsafe { BioVec::borrowed_read(buf) };
        self.submit_bio_wait(sector, BioOperation::Read, alloc::vec![vector])
            .0
    }

    /// Synchronous borrowed-buffer adapter over the owned submission contract.
    ///
    /// Accepts any buffer start address and lends the shared source until completion.
    /// Drivers cannot obtain mutable access to this source.
    ///
    /// # Errors
    /// Returns validation, wait preparation, or terminal device errors. Unwinding
    /// builds return Unsupported before lending memory.
    pub fn write_block(self: &Arc<Self>, block_id: u64, buf: &[u8]) -> DriverResult {
        let sector = block_id
            .checked_mul(self.queue.limits().logical_block_sectors())
            .ok_or(DriverError::InvalidInput)?;
        if self.is_read_only() {
            return Err(DriverError::ReadOnly);
        }
        if buf.is_empty() {
            return if sector <= self.num_sectors() {
                Ok(())
            } else {
                Err(DriverError::InvalidInput)
            };
        }
        if cfg!(panic = "unwind") {
            return Err(DriverError::Unsupported);
        }
        // SAFETY: the shared source borrow remains live until the mandatory
        // terminal wait retires all device access and disposes of the vector.
        let vector = unsafe { BioVec::borrowed_write(buf) };
        self.submit_bio_wait(sector, BioOperation::Write, alloc::vec![vector])
            .0
    }

    /// Waits for a flush through the same data submission contract.
    ///
    /// # Errors
    /// Returns wait preparation or device errors.
    pub fn flush(self: &Arc<Self>) -> DriverResult {
        self.submit_bio_wait(0, BioOperation::Flush, Vec::new()).0
    }

    /// Submits owned memory and waits using the host's existing completion mechanism.
    ///
    /// Returns status and the original owners independently, including on wait
    /// preparation, validation or device failure. Read data may be partial.
    pub fn submit_bio_wait(
        self: &Arc<Self>,
        sector: u64,
        operation: BioOperation,
        vectors: Vec<BioVec>,
    ) -> completion::BioResult {
        let (bio, waiter) =
            match completion::BioWait::prepare(self.clone(), sector, operation, vectors) {
                Ok(prepared) => prepared,
                Err((error, vectors)) => return (Err(error), vectors),
            };
        self.submit_bio(bio);
        waiter.wait()
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

    /// Transfers an owned bio to its driver after full validation and one remap.
    /// Requires sleepable task context. Validation errors also complete the bio;
    /// return does not imply completion.
    pub fn submit_bio(&self, mut bio: Bio) {
        let validate = || -> DriverResult<u64> {
            if !core::ptr::eq(self, bio.device().as_ref()) {
                return Err(DriverError::InvalidInput);
            }
            if bio.operation() == BioOperation::Write && self.is_read_only() {
                return Err(DriverError::ReadOnly);
            }
            let sectors = bio.validate()?;
            if bio.operation() == BioOperation::Flush {
                return Ok(0);
            }
            let end = bio
                .sector()
                .checked_add(sectors)
                .ok_or(DriverError::InvalidInput)?;
            let sector = self
                .start_sector
                .checked_add(bio.sector())
                .ok_or(DriverError::InvalidInput)?;
            if end > self.num_sectors()
                || !sector.is_multiple_of(self.queue.limits().logical_block_sectors())
            {
                return Err(DriverError::InvalidInput);
            }
            Ok(sector)
        };
        match validate() {
            Ok(sector) => {
                bio.remap(sector);
                self.queue.submit(bio);
            }
            Err(error) => bio.end_io(Err(error)),
        }
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

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
