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

pub struct MockTransport {
    pub device_type: DeviceType,
    pub status: RefCell<DeviceStatus>,
    pub features: u64,
    pub config_space: RefCell<[u8; 256]>,
    pub interrupt_ack_count: Arc<AtomicUsize>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::new_with_type(DeviceType::Block)
    }

    pub fn new_with_type(device_type: DeviceType) -> Self {
        Self {
            device_type,
            status: RefCell::new(DeviceStatus::empty()),
            features: 0,
            config_space: RefCell::new([0; 256]),
            interrupt_ack_count: Arc::new(AtomicUsize::new(0)),
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
        32
    }

    fn notify(&mut self, _queue: u16) {}

    fn get_status(&self) -> DeviceStatus {
        *self.status.borrow()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        *self.status.borrow_mut() = status;
    }

    fn set_guest_page_size(&mut self, _guest_page_size: u32) {}

    fn requires_legacy_layout(&self) -> bool {
        false
    }

    fn queue_set(
        &mut self,
        _queue: u16,
        _size: u32,
        _descriptors: PhysAddr,
        _driver_area: PhysAddr,
        _device_area: PhysAddr,
    ) {
    }

    fn queue_unset(&mut self, _queue: u16) {}

    fn queue_used(&mut self, _queue: u16) -> bool {
        false
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.interrupt_ack_count.fetch_add(1, Ordering::Relaxed);
        InterruptStatus::empty()
    }

    fn read_config_generation(&self) -> u32 {
        0
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T> {
        let size = core::mem::size_of::<T>();
        let config = self.config_space.borrow();
        if offset
            .checked_add(size)
            .is_none_or(|end| end > config.len())
        {
            return Err(Error::ConfigSpaceTooSmall);
        }

        T::read_from_bytes(&config[offset..offset + size]).map_err(|_| Error::ConfigSpaceTooSmall)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<()> {
        let bytes = value.as_bytes();
        let mut config = self.config_space.borrow_mut();
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
