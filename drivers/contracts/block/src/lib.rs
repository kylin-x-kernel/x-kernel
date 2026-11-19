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
    any::Any,
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
mod stats;

// #[cfg(feature = "sdmmc")]
// pub mod sdmmc;
#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use queue::{BlkMqOperations, QueueLimits, Request, RequestQueue, SECTOR_SHIFT, SECTOR_SIZE};
pub use stats::{BlockIoStats, init_io_clock};

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
        /// Establish one exclusive holder, independently of ordinary opens.
        const EXCLUSIVE = 1 << 2;
        /// Ask the driver not to wait for media during open.
        const NONBLOCK = 1 << 3;
        /// Permit legacy write ioctls without permitting data writes.
        const WRITE_IOCTL = 1 << 4;
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

    /// Opens this disk in sleepable task context under its open mutex.
    ///
    /// Called once per successful open attempt, including partition opens,
    /// as Linux `blkdev_get_whole()` calls `gendisk::fops->open`. Must not
    /// recursively open or close a view of the same disk: the mutex's task-owner
    /// check rejects recursive locking before it can sleep.
    fn open(&self, _disk: &Gendisk, _mode: BlockOpenMode) -> KResult<()> {
        Ok(())
    }

    /// Closes one successful open in sleepable task context under the open mutex.
    /// Must not recursively open or close a view of the same disk. The mutex's
    /// task-owner check rejects recursive locking before it can sleep.
    fn release(&self, _disk: &Gendisk) {}

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

    fn open(&self, disk: &Gendisk, mode: BlockOpenMode) -> KResult<()> {
        (**self).open(disk, mode)
    }

    fn release(&self, disk: &Gendisk) {
        (**self).release(disk)
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
    open_mutex: Mutex<()>,
    operations: Arc<dyn BlockDeviceOperations>,
    queue: Arc<RequestQueue>,
    part0: Weak<BlockDevice>,
    // Completion may run in IRQ/softirq context; never hold this across I/O,
    // notification, registry access, or allocation.
    io_accounting: kspin::SpinNoIrq<stats::IoAccounting>,
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
    /// Returns [`DriverError::InvalidInput`] for a name that is not a single
    /// nonempty path component (or contains ASCII controls), an invalid
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
            || matches!(name.as_str(), "." | "..")
            || name
                .bytes()
                .any(|byte| byte == b'/' || byte.is_ascii_control())
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
                open_mutex: Mutex::new(()),
                operations,
                queue,
                part0: part0.clone(),
                io_accounting: kspin::SpinNoIrq::new(stats::IoAccounting::default()),
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
    exclusive_holder: Mutex<Option<(Weak<dyn Any + Send + Sync>, usize)>>,
}

/// One open of a canonical block-device object, optionally with an exclusive holder.
///
/// Corresponds to the file lifetime managed by Linux `bdev_open()`/`bdev_release()`.
/// Retains the device and open mode without depending on publication. An
/// exclusive claim excludes other holders, while the same holder may open again
/// and ordinary opens may coexist. Weak holder references pin identity without
/// retaining the holder object, so a superblock can own its own open file.
/// Release and drop require sleepable task context with no spin/IRQ-disable/
/// preemption-disable guard. The driver is released exactly once.
pub struct BlockDeviceClaim {
    device: Arc<BlockDevice>,
    mode: BlockOpenMode,
    holder: Option<Weak<dyn Any + Send + Sync>>,
    is_active: Mutex<bool>,
}

impl BlockDeviceClaim {
    /// Returns the canonical block-device object owned by this claim.
    pub fn device(&self) -> &Arc<BlockDevice> {
        &self.device
    }

    /// Returns the mode used for this open, including exclusive ownership.
    pub const fn mode(&self) -> BlockOpenMode {
        self.mode
    }

    /// Releases the claim before the token itself is dropped.
    ///
    /// This is idempotent so an owning object's lifecycle can release block
    /// ownership at its logical death rather than waiting for its final `Arc`.
    ///
    /// # Panics
    ///
    /// Recursive open/close from the same disk's driver callback is rejected by
    /// the existing mutex task-owner check, including recursion on this token.
    pub fn release(&self) {
        // Published opens can only become inactive. Do not wait for disk
        // callbacks when dropping an already released or failed open; try_lock
        // also keeps callback recursion from blocking on another token's lock.
        if self.is_active.try_lock().is_some_and(|active| !*active) {
            return;
        }
        let _open_guard = self.device.disk.open_mutex.lock();
        let mut is_active = self.is_active.lock();
        if !*is_active {
            return;
        }
        if let Some(holder) = &self.holder {
            let mut current = self.device.exclusive_holder.lock();
            assert!(
                current
                    .as_ref()
                    .is_some_and(|(owner, count)| { Weak::ptr_eq(owner, holder) && *count > 0 }),
                "an exclusive block-device claim must own the holder"
            );
            if let Some((_, count)) = current.as_mut() {
                *count -= 1;
                if *count == 0 {
                    *current = None;
                }
            }
        }
        self.device.disk.operations.release(&self.device.disk);
        *is_active = false;
    }
}

impl Drop for BlockDeviceClaim {
    fn drop(&mut self) {
        self.release();
    }
}

impl BlockDevice {
    /// Samples disk-wide I/O statistics without waiting for outstanding I/O.
    ///
    /// May run in task or IRQ/softirq context. The short IRQ-safe critical
    /// section includes the non-sleeping clock sample, never driver callbacks.
    pub fn io_stats(&self) -> BlockIoStats {
        let mut accounting = self.disk.io_accounting.lock();
        accounting.snapshot(stats::now_ns())
    }

    fn whole(disk: Arc<Gendisk>, capacity_sectors: u64) -> Self {
        let device_number = disk.device_number();
        Self {
            queue: disk.queue.clone(),
            disk,
            device_number,
            start_sector: 0,
            num_sectors: AtomicU64::new(capacity_sectors),
            exclusive_holder: Mutex::new(None),
        }
    }

    /// Opens this canonical device and retains the driver's open lifetime.
    ///
    /// Requires sleepable task context without spin/IRQ-disable/preemption-disable
    /// guards. Driver callbacks are serialized by the disk's open mutex.
    /// Unpublished disks are usable; registry lookup is only an acquisition path.
    /// Like Linux `bdev_open()`, a writable raw open does not itself reject
    /// read-only media: writes and writable mounts enforce that policy.
    ///
    /// An explicit holder establishes exclusive ownership, as Linux's non-null
    /// `bdev_open` holder does. Reopens compare object identity, not its value;
    /// each successful open must be released before another holder can claim.
    /// The owner is retained only during this call; the returned token keeps a
    /// weak identity so storing it inside its owner cannot form a strong cycle.
    /// Without an explicit holder, `EXCLUSIVE` uses the returned open object as
    /// its own holder, like a Linux block-special file's `file::private_data`.
    ///
    /// # Errors
    ///
    /// Returns [`KError::ResourceBusy`] for a competing exclusive holder, or
    /// [`KError::OutOfRange`] if the holder's open count would overflow, or the
    /// driver's open error. Failure retains no open or holder ownership.
    ///
    /// # Panics
    ///
    /// Recursive same-disk open from a driver callback is rejected by the
    /// existing mutex task-owner check before blocking.
    pub fn open(
        self: &Arc<Self>,
        mut mode: BlockOpenMode,
        holder: Option<Arc<dyn Any + Send + Sync>>,
    ) -> KResult<Arc<BlockDeviceClaim>> {
        // Reject callback recursion before allocation can enter reclaim.
        self.disk
            .open_mutex
            .assert_not_owned_by_current("block-device open must not recurse from its driver");
        if holder.is_some() {
            mode.insert(BlockOpenMode::EXCLUSIVE);
        }
        let identity = holder.as_ref().map(Arc::downgrade);
        let claim = Arc::new_cyclic(|weak: &Weak<BlockDeviceClaim>| BlockDeviceClaim {
            device: self.clone(),
            mode,
            holder: if mode.contains(BlockOpenMode::EXCLUSIVE) {
                identity.or_else(|| Some(weak.clone()))
            } else {
                None
            },
            is_active: Mutex::new(false),
        });
        let _open_guard = self.disk.open_mutex.lock();
        let holder_count = if let Some(holder) = &claim.holder {
            match self.exclusive_holder.lock().as_ref() {
                Some((current, count)) if Weak::ptr_eq(current, holder) => {
                    count.checked_add(1).ok_or(KError::OutOfRange)?
                }
                Some(_) => return Err(KError::ResourceBusy),
                None => 1,
            }
        } else {
            0
        };
        self.disk.operations.open(&self.disk, mode)?;
        if let Some(holder) = &claim.holder {
            *self.exclusive_holder.lock() = Some((holder.clone(), holder_count));
        }
        *claim.is_active.lock() = true;
        Ok(claim)
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
        self.read_sectors(sector, buf)
    }

    /// Reads a borrowed buffer starting at a fixed 512-byte sector.
    ///
    /// Requires sleepable task context and retains the borrow until terminal
    /// completion. Queue alignment and device bounds are checked by Bio dispatch.
    ///
    /// # Errors
    /// Returns validation, wait preparation or device errors. An error may leave
    /// partial data. Unwinding builds return Unsupported before lending memory.
    pub fn read_sectors(self: &Arc<Self>, sector: u64, buf: &mut [u8]) -> DriverResult {
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
        self.write_sectors(sector, buf)
    }

    /// Writes a borrowed buffer starting at a fixed 512-byte sector.
    ///
    /// Requires sleepable task context and retains the borrow until terminal
    /// completion. Queue alignment, device bounds and read-only policy are
    /// checked by Bio dispatch.
    ///
    /// # Errors
    /// Returns validation, wait preparation or device errors. Unwinding builds
    /// return Unsupported before lending memory.
    pub fn write_sectors(self: &Arc<Self>, sector: u64, buf: &[u8]) -> DriverResult {
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
        let validate = || -> DriverResult<(u64, u64)> {
            if !core::ptr::eq(self, bio.device().as_ref()) {
                return Err(DriverError::InvalidInput);
            }
            if bio.operation() == BioOperation::Write && self.is_read_only() {
                return Err(DriverError::ReadOnly);
            }
            let sectors = bio.validate()?;
            if bio.operation() == BioOperation::Flush {
                return Ok((0, 0));
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
            Ok((sector, sectors))
        };
        match validate() {
            Ok((sector, sectors)) => {
                bio.start_accounting(sectors);
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

type BlockSubscriber = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct BlockRegistry {
    disks: BTreeMap<DeviceNumber, GendiskRef>,
    subscribers: Vec<BlockSubscriber>,
}

static BLOCK_DEVICES: Lazy<Mutex<BlockRegistry>> =
    Lazy::new(|| Mutex::new(BlockRegistry::default()));

/// Installs a permanent inventory subscriber and atomically snapshots the disks.
///
/// Requires sleepable task context. Notifications run after releasing the
/// registry lock and may coalesce concurrent changes by taking a fresh snapshot.
/// Subscribers must not perform driver I/O; kernel-lifetime services use this
/// to project identity and statistics without owning disk publication.
pub fn subscribe_block_devices(subscriber: Arc<dyn Fn() + Send + Sync>) -> Vec<Arc<BlockDevice>> {
    let mut registry = BLOCK_DEVICES.lock();
    registry.subscribers.push(subscriber);
    registry.disks.values().map(|disk| disk.part0()).collect()
}

/// Publishes a generic disk's existing whole-device `part0` view.
///
/// # Errors
///
/// Returns [`DriverError::InvalidInput`] when no major has been assigned, or
/// [`DriverError::AlreadyExists`] when the device-number range conflicts with
/// an existing disk, or when the disk name is already published.
pub fn add_disk(disk: GendiskRef) -> DriverResult<Arc<BlockDevice>> {
    if disk.major() == 0 {
        return Err(DriverError::InvalidInput);
    }
    let device_number = disk.device_number();
    let mut registry = BLOCK_DEVICES.lock();
    let first_minor = disk.first_minor();
    let last_minor = first_minor + disk.minors() - 1;
    let conflicts = registry.disks.values().any(|registered| {
        if registered.name() == disk.name() {
            return true;
        }
        if registered.major() != disk.major() {
            return false;
        }
        let registered_first = registered.first_minor();
        let registered_last = registered_first + registered.minors() - 1;
        first_minor <= registered_last && registered_first <= last_minor
    });
    if conflicts || registry.disks.contains_key(&device_number) {
        return Err(DriverError::AlreadyExists);
    }
    registry.disks.insert(device_number, disk.clone());
    let subscribers = registry.subscribers.clone();
    drop(registry);
    for subscriber in subscribers {
        subscriber();
    }
    Ok(disk.part0())
}

/// Removes a generic disk and its block-device views.
pub fn del_gendisk(device_number: DeviceNumber) -> Option<GendiskRef> {
    let (disk, subscribers) = {
        let mut registry = BLOCK_DEVICES.lock();
        let disk = registry.disks.remove(&device_number)?;
        (disk, registry.subscribers.clone())
    };
    for subscriber in subscribers {
        subscriber();
    }
    Some(disk)
}

/// Looks up a registered block-device view by `dev_t`.
pub fn lookup_block_device(device_number: DeviceNumber) -> Option<Arc<BlockDevice>> {
    BLOCK_DEVICES
        .lock()
        .disks
        .get(&device_number)
        .map(|disk| disk.part0())
}

/// Returns a snapshot of all registered block-device views.
pub fn block_devices() -> Vec<Arc<BlockDevice>> {
    BLOCK_DEVICES
        .lock()
        .disks
        .values()
        .map(|disk| disk.part0())
        .collect()
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
