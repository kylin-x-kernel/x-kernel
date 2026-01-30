//! Global page allocation helpers.
use kerrno::KResult;
use memaddr::{PhysAddr, VirtAddr};

use crate::{PAGE_SIZE, UsageKind, global_allocator};

/// A RAII wrapper for contiguous 4K-sized pages.
///
/// Automatically deallocates the pages when dropped.
#[derive(Debug)]
pub struct GlobalPage {
    start_va: VirtAddr,
    num_pages: usize,
}

impl GlobalPage {
    /// Allocates one 4K-sized page.
    pub fn alloc() -> KResult<Self> {
        Self::alloc_pages(1)
    }

    /// Allocates one 4K-sized page and initializes it with zero.
    pub fn alloc_zero() -> KResult<Self> {
        let mut page = Self::alloc()?;
        page.zero();
        Ok(page)
    }

    /// Allocates contiguous 4K-sized pages.
    pub fn alloc_contiguous(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        Self::alloc_pages_with_alignment(num_pages, align_pow2)
    }

    /// Get the start virtual address of the page.
    pub fn start_va(&self) -> VirtAddr {
        self.start_va
    }

    /// Get the start physical address by converting the virtual address.
    pub fn start_pa<F>(&self, v2p: F) -> PhysAddr
    where
        F: FnOnce(VirtAddr) -> PhysAddr,
    {
        v2p(self.start_va)
    }

    /// Get the total size (in bytes) of these pages.
    pub fn size(&self) -> usize {
        self.num_pages * PAGE_SIZE
    }

    /// Convert the start virtual address to a raw pointer.
    pub fn as_ptr(&self) -> *const u8 {
        self.start_va.as_ptr()
    }

    /// Convert the start virtual address to a mutable raw pointer.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.start_va.as_mut_ptr()
    }

    /// Fills the allocated pages with a specific byte value.
    pub fn fill(&mut self, byte: u8) {
        unsafe { core::ptr::write_bytes(self.as_mut_ptr(), byte, self.size()) }
    }

    /// Fills the allocated pages with zero.
    pub fn zero(&mut self) {
        self.fill(0)
    }

    /// Returns a slice for reading data.
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.as_ptr(), self.size()) }
    }

    /// Returns a mutable slice for writing data.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.as_mut_ptr(), self.size()) }
    }

    /// Internal function to allocate pages.
    fn alloc_pages(num_pages: usize) -> KResult<Self> {
        let va = global_allocator()
            .alloc_pages(num_pages, PAGE_SIZE, UsageKind::Global)
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => kerrno::KError::NoMemory,
                _ => kerrno::KError::InvalidInput,
            })?;
        Ok(Self {
            start_va: va.into(),
            num_pages,
        })
    }

    /// Internal function to allocate pages with specific alignment.
    fn alloc_pages_with_alignment(num_pages: usize, align_pow2: usize) -> KResult<Self> {
        let va = global_allocator()
            .alloc_pages(num_pages, align_pow2, UsageKind::Global)
            .map_err(|e| match e {
                alloc_engine::AllocError::NoMemory => kerrno::KError::NoMemory,
                _ => kerrno::KError::InvalidInput,
            })?;
        Ok(Self {
            start_va: va.into(),
            num_pages,
        })
    }
}

impl Drop for GlobalPage {
    fn drop(&mut self) {
        global_allocator().dealloc_pages(self.start_va.into(), self.num_pages, UsageKind::Global);
    }
}
