// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use kspin::SpinNoIrq;
use log::{trace, warn};
use vdev_virtio::{
    AccessWidth, AddressSpaceMemory, GuestPhysAddr, MmioReadOutcome, MmioWriteAction,
    VirtioDeviceId, VirtioError, VirtioMmioState, VirtioQueue, VirtioResult,
    memory::GuestMemoryAccessor, mmio::transport, queue::VirtQueueDesc,
};

use crate::{
    backend::BlockBackend, block::config::VirtioBlockConfig, constants::*, mmio::VirtioBlockHeader,
};

/// Action that the VMM must perform after an MMIO write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockDeviceEvent {
    /// No external action is required.
    None,
    /// The used-ring interrupt bit became pending.
    InterruptPending,
    /// A blocking backend must process the notified queue from runtime poll.
    QueuePending(u16),
    /// The guest reset the transport.
    Reset,
}

/// VirtIO MMIO Block Device
///
/// Standard MMIO register handling is delegated to [`VirtioMmioState`]; this
/// type owns only the block-specific configuration space and request data path.
///
/// # Generic Parameters
/// - `B`: Block backend implementation that handles actual storage operations
/// - `T`: Guest memory accessor with address translation capabilities
pub struct VirtioMmioBlockDevice<B: BlockBackend, T: GuestMemoryAccessor + Clone> {
    /// Shared VirtIO MMIO transport state and queues.
    state: VirtioMmioState<T>,
    /// Block device configuration (capacity, geometry, ...).
    block_config: VirtioBlockConfig,
    /// Block backend.
    backend: B,
    /// Guest memory accessor.
    accessor: Arc<T>,
    pending_head: SpinNoIrq<Option<u16>>,
}

impl<B: BlockBackend, T: GuestMemoryAccessor + Clone> VirtioMmioBlockDevice<B, T> {
    /// Create a new VirtIO MMIO block device.
    pub fn new(
        base_ipa: GuestPhysAddr,
        length: usize,
        block_backend: B,
        block_config: VirtioBlockConfig,
        translator: T,
    ) -> VirtioResult<Self> {
        let accessor = Arc::new(translator);
        let queues = vec![VirtioQueue::new(0, DEFAULT_QUEUE_SIZE, accessor.clone())];

        let state = VirtioMmioState::new(
            base_ipa,
            length,
            VirtioDeviceId::Block.to_device_id(),
            VIRTIO_VENDOR_ID,
            VIRTIO_BLK_FEATURES,
            queues,
        );

        Ok(Self {
            state,
            block_config,
            backend: block_backend,
            accessor,
            pending_head: SpinNoIrq::new(None),
        })
    }

    /// Check if device is enabled.
    pub fn is_enabled(&self) -> bool {
        true
    }

    /// Get device status.
    pub fn get_status(&self) -> u32 {
        self.state.status()
    }

    /// Returns the current VirtIO MMIO interrupt status bits.
    pub fn interrupt_status(&self) -> u32 {
        self.state.interrupt_status()
    }

    /// Set device status directly (bypasses validation; bring-up helper).
    pub fn set_status(&self, status: u32) {
        self.state.set_status(status);
    }

    /// Check if device is ready (driver has set `DRIVER_OK`).
    pub fn is_device_ready(&self) -> bool {
        self.state.is_driver_ok()
    }

    /// Handle MMIO read operations. Standard registers are served by the shared
    /// state; the block config region is interpreted here.
    pub fn mmio_read(&self, addr: GuestPhysAddr, width: AccessWidth) -> VirtioResult<usize> {
        if !self.is_enabled() {
            return Ok(0);
        }
        match self.state.mmio_read(addr, width)? {
            MmioReadOutcome::Standard(v) => Ok(v as usize),
            MmioReadOutcome::DeviceConfig { offset, width } => {
                self.read_config_space(offset, width)
            }
        }
    }

    /// Handle MMIO write operations.
    pub fn mmio_write(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
        val: usize,
    ) -> VirtioResult<BlockDeviceEvent> {
        let mut memory = AddressSpaceMemory::new(self.accessor.as_ref());
        self.mmio_write_with_memory(addr, width, val, &mut memory)
    }

    /// Handles an MMIO write using a guest-memory capability scoped to this
    /// device access.
    ///
    /// The capability backs the `QUEUE_READY` ring-layout validation as well
    /// as queue processing, so runtimes whose queues were constructed with a
    /// non-translating placeholder accessor (e.g. axvisor) can still make
    /// queues ready.
    pub fn mmio_write_with_memory(
        &self,
        addr: GuestPhysAddr,
        width: AccessWidth,
        val: usize,
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<BlockDeviceEvent> {
        match self
            .state
            .mmio_write_with_memory(addr, width, val, memory)?
        {
            MmioWriteAction::None => Ok(BlockDeviceEvent::None),
            MmioWriteAction::Reset => {
                // A transport reset invalidates every in-flight descriptor,
                // including a deferred request that was removed from avail.
                self.pending_head.lock().take();
                Ok(BlockDeviceEvent::Reset)
            }
            MmioWriteAction::InterruptPending => Ok(BlockDeviceEvent::InterruptPending),
            MmioWriteAction::QueueNotified(queue_index) => {
                if self.backend.requires_deferred_processing() {
                    Ok(BlockDeviceEvent::QueuePending(queue_index))
                } else {
                    self.handle_queue_notify(queue_index, memory)
                }
            }
        }
    }

    /// Processes one previously notified queue from a runtime context that may
    /// block while servicing the backend.
    pub fn process_pending_queue(
        &self,
        queue_index: u16,
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<BlockDeviceEvent> {
        self.handle_queue_notify(queue_index, memory)
    }

    /// Handle queue notification.
    fn handle_queue_notify(
        &self,
        queue_index: u16,
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<BlockDeviceEvent> {
        if !self.is_device_ready() {
            return Ok(BlockDeviceEvent::None);
        }
        let mut queues = self.state.queues_lock();
        let queue = queues
            .get_mut(queue_index as usize)
            .ok_or(VirtioError::InvalidQueue)?;
        if !queue.is_valid() {
            return Ok(BlockDeviceEvent::None);
        }
        let mut notify = false;
        let mut pending_head = self.pending_head.lock();
        loop {
            let head = if let Some(head) = pending_head.take() {
                head
            } else if let Some(head) = queue.pop_available_head_with_memory(memory)? {
                head
            } else {
                break;
            };
            match self.process_request(queue, head, memory) {
                Ok(Some(written)) => {
                    notify |= queue.complete_with_memory(head, written, memory)?;
                }
                Ok(None) => {
                    *pending_head = Some(head);
                    return Ok(BlockDeviceEvent::QueuePending(queue_index));
                }
                Err(error) => {
                    warn!("virtio-blk request {head} failed: {error:?}");
                    notify |= queue.complete_with_memory(head, 0, memory)?;
                }
            }
        }
        if notify {
            self.trigger_interrupt();
            Ok(BlockDeviceEvent::InterruptPending)
        } else {
            Ok(BlockDeviceEvent::None)
        }
    }

    fn process_request(
        &self,
        queue: &VirtioQueue<T>,
        head: u16,
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<Option<u32>> {
        let chain = queue.descriptor_chain_with_memory(head, memory)?;
        let descriptors = chain.descriptors();
        if descriptors.len() < MIN_DESCRIPTOR_CHAIN_LENGTH {
            return Err(VirtioError::InvalidDescriptor);
        }
        let [header, data @ .., status] = descriptors else {
            return Err(VirtioError::InvalidDescriptor);
        };
        if header.is_write() || header.len < VirtioBlockHeader::SIZE {
            return Err(VirtioError::InvalidDescriptor);
        }
        if !status.is_write() || status.len < 1 {
            return Err(VirtioError::InvalidDescriptor);
        }
        let mut header_bytes = [0u8; VirtioBlockHeader::SIZE as usize];
        memory.read(header.base_addr, &mut header_bytes)?;
        let [t0, t1, t2, t3, _, _, _, _, sector @ ..] = header_bytes;
        let request_type = u32::from_le_bytes([t0, t1, t2, t3]);
        let sector = u64::from_le_bytes(sector);
        let completion = match request_type {
            VIRTIO_BLK_T_IN => self.process_read(sector, data, memory),
            VIRTIO_BLK_T_OUT => self.process_write(sector, data, memory),
            VIRTIO_BLK_T_FLUSH if data.is_empty() => self.backend.flush().map(|()| 1),
            _ => {
                memory.write(status.base_addr, &[VIRTIO_BLK_S_UNSUPP as u8])?;
                return Ok(Some(1));
            }
        };
        match completion {
            Ok(used_len) => {
                memory.write(status.base_addr, &[VIRTIO_BLK_S_OK as u8])?;
                Ok(Some(used_len))
            }
            Err(VirtioError::WouldBlock) => Ok(None),
            Err(error) => {
                warn!("virtio-blk request {head} failed: {error:?}");
                memory.write(status.base_addr, &[VIRTIO_BLK_S_IOERR as u8])?;
                Ok(Some(1))
            }
        }
    }

    fn process_read(
        &self,
        sector: u64,
        descriptors: &[VirtQueueDesc],
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<u32> {
        if !descriptors.iter().all(|descriptor| descriptor.is_write()) {
            return Err(VirtioError::InvalidDescriptor);
        }
        let total_len = self.validate_data_request(sector, descriptors)?;
        let mut buffer = allocate_request_buffer(total_len)?;
        let bytes_read = self.backend.read(sector, &mut buffer)?;
        if bytes_read != total_len {
            return Err(VirtioError::BackendError);
        }
        let mut offset = 0;
        for descriptor in descriptors {
            let end = offset + descriptor.len as usize;
            memory.write(descriptor.base_addr, &buffer[offset..end])?;
            offset = end;
        }
        u32::try_from(total_len)
            .ok()
            .and_then(|len| len.checked_add(1))
            .ok_or(VirtioError::InvalidBufferSize)
    }

    fn process_write(
        &self,
        sector: u64,
        descriptors: &[VirtQueueDesc],
        memory: &mut dyn vdev_virtio::GuestMemory,
    ) -> VirtioResult<u32> {
        if !descriptors.iter().all(|descriptor| !descriptor.is_write()) {
            return Err(VirtioError::InvalidDescriptor);
        }
        let total_len = self.validate_data_request(sector, descriptors)?;
        let mut buffer = allocate_request_buffer(total_len)?;
        let mut offset = 0;
        for descriptor in descriptors {
            let end = offset + descriptor.len as usize;
            memory.read(descriptor.base_addr, &mut buffer[offset..end])?;
            offset = end;
        }
        let bytes_written = self.backend.write(sector, &buffer)?;
        if bytes_written != total_len {
            return Err(VirtioError::BackendError);
        }
        Ok(1)
    }

    fn validate_data_request(
        &self,
        sector: u64,
        descriptors: &[VirtQueueDesc],
    ) -> VirtioResult<usize> {
        if descriptors.len() > self.block_config.seg_max as usize
            || descriptors
                .iter()
                .any(|descriptor| descriptor.len > self.block_config.size_max)
        {
            return Err(VirtioError::InvalidBufferSize);
        }
        let total_len = descriptors.iter().try_fold(0usize, |total, descriptor| {
            total
                .checked_add(descriptor.len as usize)
                .ok_or(VirtioError::InvalidBufferSize)
        })?;
        if total_len % SECTOR_SIZE as usize != 0 {
            return Err(VirtioError::InvalidBufferSize);
        }
        let request_start = sector
            .checked_mul(SECTOR_SIZE as u64)
            .ok_or(VirtioError::InvalidSector)?;
        let request_end = request_start
            .checked_add(total_len as u64)
            .ok_or(VirtioError::InvalidSector)?;
        let capacity = self
            .block_config
            .capacity
            .checked_mul(SECTOR_SIZE as u64)
            .ok_or(VirtioError::InvalidConfig)?;
        if request_end > capacity {
            return Err(VirtioError::InvalidSector);
        }
        Ok(total_len)
    }

    /// Raise the used-buffer notification interrupt bit.
    fn trigger_interrupt(&self) {
        self.state.set_interrupt(VIRTIO_MMIO_INT_VRING);
        trace!("Triggered interrupt for used buffer notification");
    }

    /// Get the currently selected queue index, if in range.
    pub fn get_selected_queue(&self) -> Option<u16> {
        self.state.selected_queue_index()
    }

    /// Get a clone of the queue at `index`, if it exists.
    pub fn get_queue(&self, index: u16) -> Option<VirtioQueue<T>> {
        self.state.queues_lock().get(index as usize).cloned()
    }

    /// Read from block device configuration space.
    fn read_config_space(&self, offset: u64, width: AccessWidth) -> VirtioResult<usize> {
        // Block config space uses 32-bit accesses.
        transport::validate_access_width(width)?;

        let value = match offset {
            VIRTIO_BLK_CFG_CAPACITY_LOW => self.block_config.capacity as u32,
            VIRTIO_BLK_CFG_CAPACITY_HIGH => (self.block_config.capacity >> 32) as u32,
            VIRTIO_BLK_CFG_SIZE_MAX => self.block_config.size_max,
            VIRTIO_BLK_CFG_SEG_MAX => self.block_config.seg_max,
            VIRTIO_BLK_CFG_GEOMETRY => {
                (self.block_config.cylinders as u32)
                    | ((self.block_config.heads as u32) << 16)
                    | ((self.block_config.sectors as u32) << 24)
            }
            VIRTIO_BLK_CFG_BLK_SIZE => self.block_config.blk_size,
            VIRTIO_BLK_CFG_PHYSICAL_BLOCK_EXP => self.block_config.physical_block_exp as u32,
            VIRTIO_BLK_CFG_ALIGNMENT_OFFSET => self.block_config.alignment_offset as u32,
            VIRTIO_BLK_CFG_MIN_IO_SIZE => self.block_config.min_io_size as u32,
            VIRTIO_BLK_CFG_OPT_IO_SIZE => self.block_config.opt_io_size,
            _ => 0,
        };

        Ok(value as usize)
    }
}

fn allocate_request_buffer(len: usize) -> VirtioResult<alloc::vec::Vec<u8>> {
    let mut buffer = alloc::vec::Vec::new();
    buffer
        .try_reserve_exact(len)
        .map_err(|_| VirtioError::InvalidBufferSize)?;
    buffer.resize(len, 0);
    Ok(buffer)
}

unittest::path_mod_test!(tests, "../../ut-tests/device.rs");
