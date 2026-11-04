// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed pointer views over the shared coherent allocation guard.

use super::{CoherentMem, DmaOp, DmaSpec};
use crate::{ResError, ResResult};

/// A typed container holding a single `T` in coherent DMA memory.
///
/// Wraps [`CoherentMem`] with a type-safe view so drivers can treat the DMA
/// buffer as a `T` rather than raw bytes. Coherent memory needs no explicit
/// cache sync.
///
/// `T` must be DMA-safe: a plain-old-data layout (`#[repr(C)]` / no pointers),
/// `Copy`, and stable in memory. The caller is responsible for upholding this.
pub struct CoherentBox<T> {
    inner: CoherentMem,
    _marker: core::marker::PhantomData<T>,
}

impl<T> CoherentBox<T> {
    /// Allocate a coherent buffer large and aligned enough to hold a `T`.
    ///
    /// `extra_align` overrides the alignment when the device requires more
    /// than `align_of::<T>()`; pass `0` to use the type's natural alignment.
    pub fn alloc_with(provider: &'static dyn DmaOp, extra_align: usize) -> ResResult<Self> {
        let align = core::mem::align_of::<T>().max(extra_align).max(1);
        let spec = DmaSpec::new(core::mem::size_of::<T>().max(1), align);
        Ok(Self {
            inner: CoherentMem::alloc_with(provider, spec)?,
            _marker: core::marker::PhantomData,
        })
    }

    /// The device-visible bus address of the contained `T`.
    pub fn bus_addr(&self) -> u64 {
        self.inner.bus_addr()
    }

    /// A mutable pointer to the CPU-visible `T`.
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.as_ptr() as *mut T
    }

    /// The buffer length in bytes.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A typed container holding `count` consecutive `T` values in coherent DMA
/// memory.
///
/// See [`CoherentBox`] for the DMA-safety contract on `T`.
pub struct CoherentArray<T> {
    inner: CoherentMem,
    count: usize,
    _marker: core::marker::PhantomData<T>,
}

impl<T> CoherentArray<T> {
    /// Allocate a coherent buffer holding `count` values of `T`.
    ///
    /// `extra_align` overrides the alignment when the device requires more
    /// than `align_of::<T>()`; pass `0` for the type's natural alignment.
    pub fn alloc_with(
        provider: &'static dyn DmaOp,
        count: usize,
        extra_align: usize,
    ) -> ResResult<Self> {
        let align = core::mem::align_of::<T>().max(extra_align).max(1);
        let len = core::mem::size_of::<T>()
            .checked_mul(count)
            .ok_or(ResError::InvalidResource)?;
        let spec = DmaSpec::new(len.max(1), align);
        Ok(Self {
            inner: CoherentMem::alloc_with(provider, spec)?,
            count,
            _marker: core::marker::PhantomData,
        })
    }

    /// The device-visible bus address of element `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= count`. Without this check an out-of-range index
    /// would yield a bus address outside the allocated buffer, risking DMA
    /// corruption of neighbouring memory.
    pub fn bus_addr_of(&self, index: usize) -> u64 {
        assert!(index < self.count, "CoherentArray index out of bounds");
        self.inner.bus_addr() + (index as u64) * (core::mem::size_of::<T>() as u64)
    }

    /// A mutable pointer to the start of the CPU-visible array.
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.as_ptr() as *mut T
    }

    /// Number of elements in the array.
    pub fn count(&self) -> usize {
        self.count
    }
}
