use core::{cell::RefCell, ptr::NonNull};

use virtio_drivers::{
    BufferDirection, Hal, PhysAddr, Result,
    transport::{DeviceStatus, DeviceType, Transport},
};

extern crate alloc;
use alloc::alloc::{Layout, alloc, dealloc};

pub struct MockHal;

// MockHal implementation for unit testing
unsafe impl Hal for MockHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            panic!("MockHal: dma_alloc failed");
        }
        unsafe { ptr.write_bytes(0, pages * 4096) }; // Zero memory
        (ptr as usize, NonNull::new(ptr).unwrap())
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        unsafe { dealloc(paddr as *mut u8, layout) };
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        buffer.as_ptr() as *mut u8 as usize
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {}
}

pub struct MockTransport {
    pub device_type: DeviceType,
    pub status: RefCell<DeviceStatus>,
    pub features: u64,
    pub config_space: RefCell<[u8; 256]>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self {
            device_type: DeviceType::Block,
            status: RefCell::new(DeviceStatus::empty()),
            features: 0,
            config_space: RefCell::new([0; 256]),
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

    fn ack_interrupt(&mut self) -> bool {
        false
    }

    fn config_space<T: 'static>(&self) -> Result<NonNull<T>> {
        unsafe {
            let ptr = self.config_space.borrow_mut().as_mut_ptr() as *mut T;
            Ok(NonNull::new_unchecked(ptr))
        }
    }
}
