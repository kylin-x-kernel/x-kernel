//! A RAM disk driver backed by heap-allocated memory.

extern crate alloc;

use alloc::alloc::{alloc_zeroed, dealloc};
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

const BLOCK_SIZE: usize = 512;

/// A RAM disk structure backed by heap memory.
pub struct RamDisk(NonNull<[u8]>);

unsafe impl Send for RamDisk {}
unsafe impl Sync for RamDisk {}

impl Default for RamDisk {
    fn default() -> Self {
        // Initially creates an empty dangling pointer for the RamDisk
        Self(NonNull::<[u8; 0]>::dangling())
    }
}

impl RamDisk {
    /// Creates a new RAM disk with the specified size hint.
    ///
    /// The size is rounded up to be aligned to the block size (512 bytes).
    pub fn new(size_hint: usize) -> Self {
        let size = align_up(size_hint);
        let layout = unsafe { Layout::from_size_align_unchecked(size, BLOCK_SIZE) };

        // Allocate the memory and create a NonNull pointer to the RAM disk buffer.
        let ptr = unsafe { NonNull::new_unchecked(alloc_zeroed(layout)) };

        Self(NonNull::slice_from_raw_parts(ptr, size))
    }
}

impl Drop for RamDisk {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }

        // Deallocate the memory when the RamDisk goes out of scope
        unsafe {
            dealloc(
                self.0.cast::<u8>().as_ptr(),
                Layout::from_size_align_unchecked(self.0.len(), BLOCK_SIZE),
            );
        }
    }
}

impl Deref for RamDisk {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // Dereferencing the RamDisk to get a slice of bytes
        unsafe { self.0.as_ref() }
    }
}

impl DerefMut for RamDisk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Dereferencing mutably for mutable operations
        unsafe { self.0.as_mut() }
    }
}

impl From<&[u8]> for RamDisk {
    fn from(data: &[u8]) -> Self {
        let mut ramdisk = RamDisk::new(data.len());
        ramdisk[..data.len()].copy_from_slice(data);
        ramdisk
    }
}

impl DriverOps for RamDisk {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "ramdisk"
    }
}

impl BlockDriverOps for RamDisk {
    #[inline]
    fn num_blocks(&self) -> u64 {
        // Calculates the number of blocks in the RAM disk
        (self.len() / BLOCK_SIZE) as u64
    }

    #[inline]
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len() {
            return Err(DriverError::Io);
        }
        buf.copy_from_slice(&self[offset..offset + buf.len()]);
        Ok(())
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len() {
            return Err(DriverError::Io);
        }
        self[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

/// Aligns a given size upwards to the nearest multiple of `BLOCK_SIZE`.
const fn align_up(val: usize) -> usize {
    (val + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1)
}
