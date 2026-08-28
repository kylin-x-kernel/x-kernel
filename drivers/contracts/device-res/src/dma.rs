// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DMA resource types: direction/spec/allocation/mapping, the [`DmaOp`]
//! capability trait, the [`DmaCoherent`] RAII handle, and the typed containers
//! [`CoherentBox`] / [`CoherentArray`].

use core::ptr::NonNull;

use crate::{ResError, ResResult};

/// Direction of a DMA transfer.
///
/// OS-neutral. Host kernels translate this into their own direction
/// representation when performing cache maintenance or bounce buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU writes, device reads (host flushes before device access).
    DriverToDevice,
    /// Device writes, CPU reads (host invalidates before CPU access).
    DeviceToDriver,
    /// Bidirectional (host flushes and invalidates as needed).
    Bidirectional,
}

/// A request for a coherent DMA buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaSpec {
    /// Buffer length in bytes.
    pub len: usize,
    /// Required alignment in bytes (power of two).
    pub align: usize,
    /// Transfer direction, used by streaming mappings and cache sync.
    pub direction: DmaDirection,
}

impl DmaSpec {
    /// Construct a spec with [`DmaDirection::Bidirectional`] (legacy default).
    pub const fn new(len: usize, align: usize) -> Self {
        Self {
            len,
            align,
            direction: DmaDirection::Bidirectional,
        }
    }

    /// Builder: override the transfer direction.
    pub const fn with_direction(mut self, d: DmaDirection) -> Self {
        self.direction = d;
        self
    }
}

/// A coherent DMA allocation returned by [`DmaOp::alloc_coherent`].
#[derive(Debug, Clone, Copy)]
pub struct DmaAllocation {
    /// Virtual address the CPU uses to access the buffer.
    pub cpu_addr: usize,
    /// Bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// The originating allocation request.
    pub spec: DmaSpec,
}

/// A streaming DMA mapping returned by [`DmaOp::map_streaming`].
#[derive(Debug, Clone, Copy)]
pub struct DmaMapping {
    /// CPU virtual address of the mapped buffer.
    pub cpu_addr: usize,
    /// Device bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// Mapped length in bytes.
    pub len: usize,
    /// Transfer direction the mapping was established with.
    pub direction: DmaDirection,
}

/// DMA capability: coherent allocation, streaming mapping, and cache
/// maintenance for non-coherent hosts.
///
/// A host kernel implements this and passes it to its driver framework. The
/// streaming / cache methods have default no-op / `Unsupported`
/// implementations so a host only overrides what its DMA model needs (e.g. a
/// bounce-buffer host leaves cache sync empty).
pub trait DmaOp: Sync {
    /// Allocate a coherent DMA buffer.
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation>;

    /// Free a coherent DMA buffer previously returned by [`Self::alloc_coherent`].
    fn free_coherent(&self, alloc: DmaAllocation);

    /// Map an existing caller-owned buffer for streaming DMA.
    ///
    /// Unlike [`Self::alloc_coherent`], the buffer is not owned by the DMA
    /// layer; the mapping is temporary and reversed by [`Self::unmap_streaming`].
    /// Hosts without explicit streaming support may implement this via bounce
    /// buffering. Default: [`Unsupported`](ResError::Unsupported).
    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let _ = (buffer, direction);
        Err(ResError::Unsupported)
    }

    /// Release a streaming mapping previously established by
    /// [`Self::map_streaming`]. Default: no-op.
    fn unmap_streaming(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the device reads it (CPU →
    /// device). Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_device(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the CPU reads it (device → CPU).
    /// Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_cpu(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Explicitly flush a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn flush_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }

    /// Explicitly invalidate a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn invalidate_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }
}

/// RAII handle to a coherent DMA buffer.
///
/// Dropping the handle frees the buffer through the provider that created it.
pub struct DmaCoherent {
    provider: Option<&'static dyn DmaOp>,
    allocation: Option<DmaAllocation>,
}

impl DmaCoherent {
    /// Allocate a coherent DMA buffer with an explicit provider.
    pub fn alloc_with(provider: &'static dyn DmaOp, spec: DmaSpec) -> ResResult<Self> {
        let allocation = provider.alloc_coherent(spec)?;
        Ok(Self {
            provider: Some(provider),
            allocation: Some(allocation),
        })
    }

    /// The CPU-visible virtual address of the buffer.
    pub fn cpu_ptr(&self) -> NonNull<u8> {
        NonNull::new(
            self.allocation
                .as_ref()
                .expect("DmaCoherent handle used after release")
                .cpu_addr as *mut u8,
        )
        .expect("DmaCoherent stored a null CPU address")
    }

    /// The device-visible bus address of the buffer.
    pub fn bus_addr(&self) -> u64 {
        self.allocation
            .as_ref()
            .expect("DmaCoherent handle used after release")
            .bus_addr
    }

    /// The buffer length in bytes.
    pub fn len(&self) -> usize {
        self.allocation
            .as_ref()
            .expect("DmaCoherent handle used after release")
            .spec
            .len
    }

    /// Returns `true` if the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for DmaCoherent {
    fn drop(&mut self) {
        if let (Some(allocation), Some(provider)) = (self.allocation.take(), self.provider.take()) {
            provider.free_coherent(allocation);
        }
    }
}

impl core::fmt::Debug for DmaCoherent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaCoherent")
            .field("allocation", &self.allocation)
            .finish_non_exhaustive()
    }
}

/// A typed container holding a single `T` in coherent DMA memory.
///
/// Wraps [`DmaCoherent`] with a type-safe view so drivers can treat the DMA
/// buffer as a `T` rather than raw bytes. Coherent memory needs no explicit
/// cache sync.
///
/// `T` must be DMA-safe: a plain-old-data layout (`#[repr(C)]` / no pointers),
/// `Copy`, and stable in memory. The caller is responsible for upholding this.
pub struct CoherentBox<T> {
    inner: DmaCoherent,
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
            inner: DmaCoherent::alloc_with(provider, spec)?,
            _marker: core::marker::PhantomData,
        })
    }

    /// The device-visible bus address of the contained `T`.
    pub fn bus_addr(&self) -> u64 {
        self.inner.bus_addr()
    }

    /// A mutable pointer to the CPU-visible `T`.
    pub fn as_mut_ptr(&self) -> *mut T {
        self.inner.cpu_ptr().as_ptr() as *mut T
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
    inner: DmaCoherent,
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
            inner: DmaCoherent::alloc_with(provider, spec)?,
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
        self.inner.cpu_ptr().as_ptr() as *mut T
    }

    /// Number of elements in the array.
    pub fn count(&self) -> usize {
        self.count
    }
}
