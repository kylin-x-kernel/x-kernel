// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible CPU-only aligned storage, separate from DMA resources.

use core::{alloc::Layout, ptr::NonNull};

use crate::{ResError, ResResult};

/// Host capability for ordinary aligned CPU memory.
///
/// Acquiring this storage must not establish a DMA mapping, change memory
/// cache attributes, or invoke device-sharing hooks. Allocation may fail and
/// must return an error instead of invoking a fatal global-allocator policy.
/// Callers must use a context in which the host allocator is available.
///
/// # Safety
///
/// A successful allocation must be non-null, exclusively owned, and valid for
/// the complete requested size and alignment until its matching deallocation.
/// Live allocations must not overlap. The provider must release an allocation
/// exactly once when given its original pointer and layout. Implementations
/// must synchronize concurrent calls to the same provider.
pub unsafe trait CpuMemoryOp: Sync {
    /// Acquire uninitialized ordinary memory for a nonempty layout.
    ///
    /// # Errors
    /// Returns `NoMemory` on exhaustion or `InvalidResource` for an unsupported
    /// layout. No allocation is transferred to the caller on error.
    fn alloc_cpu(&self, layout: Layout) -> ResResult<NonNull<u8>>;

    /// Release an allocation obtained from this provider.
    ///
    /// # Safety
    /// `pointer` and `layout` must identify a still-live allocation returned by
    /// this provider. All borrows must have ended; release it exactly once.
    unsafe fn free_cpu(&self, pointer: NonNull<u8>, layout: Layout);
}

/// Initialized aligned CPU memory owned by its original provider.
///
/// This handle grants no device bus address or DMA/cache guarantees. Its
/// provider borrow and exclusive byte view keep storage live and non-aliasing.
pub struct CpuBuffer<'a> {
    provider: &'a dyn CpuMemoryOp,
    pointer: NonNull<u8>,
    layout: Layout,
}

impl<'a> CpuBuffer<'a> {
    /// Allocate and zero CPU storage with an explicit provider.
    ///
    /// # Errors
    /// Rejects a zero-size layout with `InvalidResource`; otherwise propagates
    /// acquisition errors without exposing uninitialized memory.
    pub fn alloc_with(provider: &'a dyn CpuMemoryOp, layout: Layout) -> ResResult<Self> {
        if layout.size() == 0 {
            return Err(ResError::InvalidResource);
        }
        let pointer = provider.alloc_cpu(layout)?;
        // SAFETY: CpuMemoryOp's unsafe implementation contract supplies a live,
        // exclusive allocation of layout.size() bytes. No reference or device
        // access exists yet; initialize every byte before exposing a slice.
        unsafe { pointer.as_ptr().write_bytes(0, layout.size()) };
        Ok(Self {
            provider,
            pointer,
            layout,
        })
    }

    /// Borrow the initialized bytes exclusively for the mutable handle borrow.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: alloc_with initialized the complete owned allocation. The
        // mutable handle borrow excludes other byte views, and the provider
        // cannot reclaim it until this handle drops after that borrow ends.
        unsafe { core::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.layout.size()) }
    }
}

impl Drop for CpuBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: these are the original pointer, layout and provider from the
        // sole successful acquisition; all byte borrows have ended at drop.
        unsafe { self.provider.free_cpu(self.pointer, self.layout) };
    }
}

unittest::path_mod_test!(tests, "../ut-tests/memory.rs");
