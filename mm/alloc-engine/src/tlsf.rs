// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The TLSF (Two-Level Segregated Fit) dynamic memory allocation algorithm.
//!
//! This module wraps the implementation provided by the [rlsf] crate.

use core::{alloc::Layout, ptr::NonNull};

use rlsf::Tlsf;

use super::{AllocError, AllocResult, BaseAllocator, ByteAllocator};

/// A TLSF (Two-Level Segregated Fit) memory allocator.
///
/// It's just a wrapper structure of [`rlsf::Tlsf`], with `FLLEN` and `SLLEN`
/// fixed to 28 and 32.
pub struct TlsfByteAllocator {
    inner: Tlsf<'static, u32, u32, 28, 32>, // max pool size: 32 * 2^28 = 8G
    pool_bytes: usize,
    used_bytes: usize,
}

impl TlsfByteAllocator {
    /// Creates a new empty [`TlsfByteAllocator`].
    pub const fn new() -> Self {
        Self {
            inner: Tlsf::new(),
            pool_bytes: 0,
            used_bytes: 0,
        }
    }
}

impl Default for TlsfByteAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseAllocator for TlsfByteAllocator {
    fn init_region(&mut self, start: usize, size: usize) {
        unsafe {
            let pool = core::slice::from_raw_parts_mut(start as *mut u8, size);
            self.inner
                .insert_free_block_ptr(NonNull::new(pool).unwrap())
                .unwrap();
        }
        self.pool_bytes = size;
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        unsafe {
            let pool = core::slice::from_raw_parts_mut(start as *mut u8, size);
            self.inner
                .insert_free_block_ptr(NonNull::new(pool).unwrap())
                .ok_or(AllocError::InvalidInput)?;
        }
        self.pool_bytes += size;
        Ok(())
    }
}

impl ByteAllocator for TlsfByteAllocator {
    fn allocate(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let ptr = self.inner.allocate(layout).ok_or(AllocError::NoMemory)?;
        self.used_bytes += layout.size();
        Ok(ptr)
    }

    fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { self.inner.deallocate(ptr, layout.align()) }
        self.used_bytes -= layout.size();
    }

    fn total_bytes(&self) -> usize {
        self.pool_bytes
    }

    fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    fn available_bytes(&self) -> usize {
        self.pool_bytes - self.used_bytes
    }
}
