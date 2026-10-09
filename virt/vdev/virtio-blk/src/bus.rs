// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMIO-bus binding for the ported VirtIO MMIO block device model.
//!
//! Wraps [`VirtioMmioBlockDevice`] as a [`vdev_core::MmioDevice`] on the shared
//! MMIO bus: guest memory is served through a [`GuestDma`] translation, used-ring
//! notifications are mapped to an [`IrqSender`], and the default backend is a
//! synchronous in-memory RAM disk (so no deferred queue processing is needed).
//!
//! The default MMIO window follows the QEMU virt-platform `virtio_mmio`
//! layout: slot 0 (`0x0a00_0000`) is occupied by `virt/vdev/virtio-net`,
//! so the block device takes slot 1 (`0x0a00_1000`) with guest IRQ line 49.

use alloc::{sync::Arc, vec::Vec};
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use kspin::SpinNoIrq;
use vdev_core::{GuestDma, IrqSender, MmioDevice};
use vdev_virtio::{
    AccessWidth, DmaGuestMemory, GuestPhysAddr, NoGuestMemoryAccessor, VirtioError, VirtioResult,
};

use crate::{BlockBackend, BlockDeviceEvent, VirtioBlockConfig, VirtioMmioBlockDevice};

/// Default MMIO base of the virtio-blk device (QEMU virt `virtio_mmio` slot 1).
pub const VIRTIO_BLK_BASE: u64 = 0x0a00_1000;
/// Size of the MMIO window (standard VirtIO MMIO register block).
pub const VIRTIO_BLK_SIZE: u64 = 0x200;
/// Guest IRQ line used by the virtio-blk device (QEMU virt slot 1).
pub const VIRTIO_BLK_IRQ: u32 = 49;
/// Default RAM-disk capacity in bytes.
pub const DEFAULT_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;

const SECTOR_SIZE: usize = 512;

/// Synchronous in-memory block backend.
///
/// `requires_deferred_processing` is `false`, so a queue notification is
/// serviced inline on the MMIO write path and the guest is interrupted
/// immediately when the used ring advances. The data path runs under the
/// VirtIO queue's `SpinNoIrq` lock, so the backing store is itself protected by
/// a `SpinNoIrq` spinlock: a sleeping `ksync::Mutex` here could block while
/// local interrupts are disabled and deadlock.
pub struct RamDiskBackend {
    bytes: SpinNoIrq<Vec<u8>>,
    capacity_sectors: u64,
}

impl RamDiskBackend {
    /// Create a zeroed RAM disk with the given byte capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity_bytes` is not a positive multiple of the 512-byte
    /// sector size (a latent configuration error, not a guest-visible one).
    pub fn new(capacity_bytes: u64) -> Self {
        assert!(
            capacity_bytes > 0 && capacity_bytes.is_multiple_of(SECTOR_SIZE as u64),
            "virtio-blk RAM disk capacity must be a positive multiple of 512 bytes"
        );
        Self {
            bytes: SpinNoIrq::new(alloc::vec![0u8; capacity_bytes as usize]),
            capacity_sectors: capacity_bytes / SECTOR_SIZE as u64,
        }
    }

    /// The backend capacity in 512-byte sectors.
    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    /// Validate a `[sector, sector + len)` request against `buffer_len`.
    fn checked_range(sector: u64, len: usize, buffer_len: usize) -> VirtioResult<Range<usize>> {
        let start = usize::try_from(sector)
            .ok()
            .and_then(|s| s.checked_mul(SECTOR_SIZE))
            .ok_or(VirtioError::InvalidSector)?;
        let end = start.checked_add(len).ok_or(VirtioError::InvalidSector)?;
        if end > buffer_len {
            return Err(VirtioError::InvalidSector);
        }
        Ok(start..end)
    }
}

impl BlockBackend for RamDiskBackend {
    fn requires_deferred_processing(&self) -> bool {
        false
    }

    fn read(&self, sector: u64, buffer: &mut [u8]) -> VirtioResult<usize> {
        let bytes = self.bytes.lock();
        let range = Self::checked_range(sector, buffer.len(), bytes.len())?;
        buffer.copy_from_slice(&bytes[range]);
        Ok(buffer.len())
    }

    fn write(&self, sector: u64, buffer: &[u8]) -> VirtioResult<usize> {
        let mut bytes = self.bytes.lock();
        let range = Self::checked_range(sector, buffer.len(), bytes.len())?;
        bytes[range].copy_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&self) -> VirtioResult<()> {
        Ok(())
    }
}

/// virtio-blk device bound to the VM MMIO bus.
///
/// MMIO reads go to the standard transport + block config space; MMIO writes
/// are serviced with a scoped [`vdev_virtio::GuestMemory`] capability built from
/// the VM DMA translation (the same design the ported `mmio_write_with_memory`
/// exists for), and used-ring notifications are forwarded to `irq_sender`.
pub struct VirtioBlkDevice {
    base: u64,
    size: u64,
    irq: u32,
    irq_sender: Arc<dyn IrqSender>,
    dma: Arc<dyn GuestDma>,
    model: Arc<VirtioMmioBlockDevice<RamDiskBackend, NoGuestMemoryAccessor>>,
    /// Defensive latch: a synchronous backend never defers queue processing;
    /// if this ever fires the backend policy changed and polling is required.
    deferred_pending: AtomicBool,
}

impl VirtioBlkDevice {
    /// Create the device with a RAM-disk backend of `capacity_bytes`.
    ///
    /// `irq_sender` and `dma` are the VM's interrupt-injection and guest-memory
    /// translation capabilities.
    pub fn new(
        irq_sender: Arc<dyn IrqSender>,
        dma: Arc<dyn GuestDma>,
        capacity_bytes: u64,
    ) -> Self {
        let backend = RamDiskBackend::new(capacity_bytes);
        let config = VirtioBlockConfig {
            capacity: backend.capacity_sectors(),
            ..VirtioBlockConfig::default()
        };
        // The constructor only fails on queue-size validation; the ported
        // defaults (256 entries) are always valid, so this is an impossible
        // state rather than a recoverable runtime error.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: VirtioMmioBlockDevice::new fails only \
                      on queue-size validation, and the default 256-entry queues are valid"
        )]
        let model = Arc::new(
            VirtioMmioBlockDevice::new(
                GuestPhysAddr::from_usize(VIRTIO_BLK_BASE as usize),
                VIRTIO_BLK_SIZE as usize,
                backend,
                config,
                NoGuestMemoryAccessor,
            )
            .expect("virtio-blk device construction with default queue size cannot fail"),
        );
        Self {
            base: VIRTIO_BLK_BASE,
            size: VIRTIO_BLK_SIZE,
            irq: VIRTIO_BLK_IRQ,
            irq_sender,
            dma,
            model,
            deferred_pending: AtomicBool::new(false),
        }
    }

    fn device_address(base: u64, offset: u64) -> GuestPhysAddr {
        GuestPhysAddr::from_usize((base + offset) as usize)
    }
}

impl MmioDevice for VirtioBlkDevice {
    fn name(&self) -> &str {
        "virtio-blk"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (self.base, self.size)
    }

    fn read(&self, offset: u64, size: u8) -> u64 {
        let Some(width) = access_width(size) else {
            return 0;
        };
        self.model
            .mmio_read(Self::device_address(self.base, offset), width)
            .unwrap_or_default() as u64
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        self.write_for_vcpu(offset, size, value, 0);
    }

    fn write_for_vcpu(&mut self, offset: u64, size: u8, value: u64, vcpu_id: u32) {
        let Some(width) = access_width(size) else {
            return;
        };
        let mut memory = DmaGuestMemory::new(&*self.dma);
        let event = match self.model.mmio_write_with_memory(
            Self::device_address(self.base, offset),
            width,
            value as usize,
            &mut memory,
        ) {
            Ok(event) => event,
            Err(error) => {
                log::warn!(
                    "[virtio-blk] MMIO write @ {:#x} failed: {error:?}",
                    self.base + offset,
                );
                return;
            }
        };
        match event {
            BlockDeviceEvent::InterruptPending => {
                self.irq_sender.inject(vcpu_id, self.irq);
            }
            BlockDeviceEvent::QueuePending(_) => {
                // The RAM-disk backend is synchronous, so the device data path
                // finishes during the notifying MMIO write. Reaching here means
                // the backend policy changed; the notify must then be serviced
                // by a polling mechanism, which KVMM's MmioDevice has none of.
                log::warn!("[virtio-blk] queue notification deferred; backend requires polling",);
                self.deferred_pending.store(true, Ordering::Release);
            }
            BlockDeviceEvent::Reset => {
                self.deferred_pending.store(false, Ordering::Release);
            }
            BlockDeviceEvent::None => {}
        }
    }
}

/// Map an MMIO bus access width to the VirtIO width enum.
fn access_width(size: u8) -> Option<AccessWidth> {
    match size {
        1 => Some(AccessWidth::Byte),
        2 => Some(AccessWidth::Word),
        4 => Some(AccessWidth::Dword),
        8 => Some(AccessWidth::Qword),
        _ => {
            log::warn!("[virtio-blk] unsupported MMIO access width {size}");
            None
        }
    }
}
