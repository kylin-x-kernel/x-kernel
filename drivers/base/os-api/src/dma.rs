// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw DMA operations and owned coherent memory.

use core::{fmt, ptr::NonNull};

use crate::{ResError, ResResult};

mod typed;
pub use typed::{CoherentArray, CoherentBox};

/// Coherent DMA memory; freed on drop.
pub struct CoherentMem {
    provider: &'static dyn DmaOp,
    allocation: DmaAllocation,
}

impl fmt::Debug for CoherentMem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoherentMem")
            .field("bus", &self.bus_addr())
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl CoherentMem {
    /// Acquire coherent storage and retain the exact provider allocation token.
    ///
    /// Device access must stop before drop. This preserves the provider's policy;
    /// it does not infer a device DMA view.
    pub fn alloc_with(provider: &'static dyn DmaOp, spec: DmaSpec) -> ResResult<Self> {
        let allocation = provider.alloc_coherent(spec)?;
        Ok(Self {
            provider,
            allocation,
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.allocation.cpu_addr as *mut u8
    }

    pub const fn bus_addr(&self) -> u64 {
        self.allocation.bus_addr
    }

    pub const fn len(&self) -> usize {
        self.allocation.spec.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for CoherentMem {
    fn drop(&mut self) {
        self.provider.free_coherent(self.allocation);
    }
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

/// Raw DMA allocation/mapping contract used by static HAL integrations.
///
/// Raw callers retain the original release tuple, buffer lifetime and cache
/// synchronization obligations. This contract does not infer a device window
/// from ambient state.
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

unittest::path_mod_test!(coherent_tests, "../ut-tests/coherent.rs");
