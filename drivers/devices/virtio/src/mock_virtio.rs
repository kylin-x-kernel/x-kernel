// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::RefCell,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use virtio_drivers::{
    BufferDirection, Error, Hal, PhysAddr, Result,
    transport::{DeviceStatus, DeviceType, InterruptStatus, Transport},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

#[cfg(feature = "block")]
pub use crate::blk::{BlockTestHal, BlockTestHardware, BlockTestTransport, block_test_disk};

extern crate alloc;
use alloc::{
    alloc::{Layout, alloc, dealloc},
    sync::Arc,
};

pub struct MockHal;

// SAFETY: MockHal is only used in unit tests. It uses the global allocator
// for DMA, which is safe in std/test environments. Physical and virtual
// addresses are identical in this mock (identity mapping).
unsafe impl Hal for MockHal {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: test scenario: MockHal backs DMA with the \
                  global allocator and the Hal callback has no error channel for allocation \
                  failure"
    )]
    fn dma_alloc(
        pages: usize,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        // SAFETY: Layout is guaranteed valid (power-of-2 alignment, non-zero size).
        // The returned pointer is checked for null before use.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            panic!("MockHal: dma_alloc failed");
        }
        // SAFETY: ptr is non-null and points to `pages * 4096` bytes of
        // allocated memory, so write_bytes is within bounds.
        unsafe { ptr.write_bytes(0, pages * 4096) };
        (ptr as PhysAddr, NonNull::new(ptr).unwrap())
    }

    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        _vaddr: NonNull<u8>,
        pages: usize,
        _access_platform: bool,
    ) -> i32 {
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        // SAFETY: paddr was returned by dma_alloc and points to memory
        // allocated with the same layout.
        unsafe { dealloc(paddr as *mut u8, layout) };
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // SAFETY: In the mock environment, physical and virtual addresses
        // are identical (identity mapping). The caller ensures paddr is valid.
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> PhysAddr {
        // SAFETY: In the mock, sharing is a no-op; the physical address
        // equals the virtual address.
        buffer.as_ptr() as *mut u8 as PhysAddr
    }

    unsafe fn unshare(
        _paddr: PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: BufferDirection,
        _access_platform: bool,
    ) {
    }
}

/// Selector-aware config-space responder used by multi-query probes.
type ConfigHandler = dyn Fn(u8, u8, usize) -> u8 + Send + Sync;

pub struct MockTransport {
    pub device_type: DeviceType,
    pub max_queue_size: u32,
    /// Optional selector-aware config-space responder for multi-query probes.
    pub config_handler: Option<Arc<ConfigHandler>>,
    pub status: RefCell<DeviceStatus>,
    /// Every status written, in order, shared with the test that observes it.
    pub status_history: Arc<kspin::SpinNoIrq<alloc::vec::Vec<DeviceStatus>>>,
    pub features: u64,
    pub config_space: Arc<kspin::SpinNoIrq<[u8; 256]>>,
    pub config_read_count: Arc<AtomicUsize>,
    pub interrupt_ack_count: Arc<AtomicUsize>,
    pub interrupt_status: Arc<AtomicUsize>,
    pub queue: Option<Arc<kspin::SpinNoIrq<MockQueue>>>,
}

/// Optional ring addresses for tests that emulate a device on real descriptors.
#[derive(Default)]
pub struct MockQueue {
    pub size: usize,
    pub descriptors: usize,
    pub available: usize,
    pub used: usize,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::new_with_type(DeviceType::Block)
    }

    pub fn new_with_type(device_type: DeviceType) -> Self {
        Self {
            device_type,
            max_queue_size: 256,
            config_handler: None,
            status: RefCell::new(DeviceStatus::empty()),
            status_history: Arc::new(kspin::SpinNoIrq::new(alloc::vec::Vec::new())),
            features: 0,
            config_space: Arc::new(kspin::SpinNoIrq::new([0; 256])),
            config_read_count: Arc::new(AtomicUsize::new(0)),
            interrupt_ack_count: Arc::new(AtomicUsize::new(0)),
            interrupt_status: Arc::new(AtomicUsize::new(0)),
            queue: None,
        }
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for MockTransport {
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    fn read_device_features(&mut self) -> u64 {
        self.features
    }

    fn write_driver_features(&mut self, _features: u64) {}

    fn max_queue_size(&mut self, _queue: u16) -> u32 {
        // Large enough for every device implementation under test; for example
        // the vsock queues are 128 entries. Tests may lower this per transport.
        self.max_queue_size
    }

    fn notify(&mut self, _queue: u16) {}

    fn get_status(&self) -> DeviceStatus {
        *self.status.borrow()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        *self.status.borrow_mut() = status;
        self.status_history.lock().push(status);
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {}

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        _queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        if let Some(queue) = &self.queue {
            *queue.lock() = MockQueue {
                size: size as usize,
                descriptors: descriptors as usize,
                available: driver_area as usize,
                used: device_area as usize,
            };
        }
    }

    fn queue_unset(&mut self, _queue: u16) {
        if let Some(queue) = &self.queue {
            *queue.lock() = MockQueue::default();
        }
    }

    fn queue_used(&mut self, _queue: u16) -> bool {
        false
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.interrupt_ack_count.fetch_add(1, Ordering::Relaxed);
        InterruptStatus::from_bits_truncate(self.interrupt_status.swap(0, Ordering::AcqRel) as u32)
    }

    fn read_config_generation(&self) -> u32 {
        0
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T> {
        self.config_read_count.fetch_add(1, Ordering::Relaxed);
        let size = core::mem::size_of::<T>();
        // Capture the selector before invoking the handler: a handler may
        // inspect the same config space, so the lock must not be held across
        // the callback.
        let (select, subsel) = {
            let config = self.config_space.lock();
            if offset
                .checked_add(size)
                .is_none_or(|end| end > config.len())
            {
                return Err(Error::ConfigSpaceTooSmall);
            }
            (config[0], config[1])
        };

        if let Some(handler) = self.config_handler.as_deref() {
            let bytes: alloc::vec::Vec<u8> = (0..size)
                .map(|index| handler(select, subsel, offset + index))
                .collect();
            return T::read_from_bytes(&bytes).map_err(|_| Error::ConfigSpaceTooSmall);
        }

        let config = self.config_space.lock();
        T::read_from_bytes(&config[offset..offset + size]).map_err(|_| Error::ConfigSpaceTooSmall)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<()> {
        let bytes = value.as_bytes();
        let mut config = self.config_space.lock();
        if offset
            .checked_add(bytes.len())
            .is_none_or(|end| end > config.len())
        {
            return Err(Error::ConfigSpaceTooSmall);
        }

        config[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}
