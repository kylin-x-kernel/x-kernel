// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw DMA operations, owned coherent memory and device DMA views.

use core::{fmt, ptr::NonNull};

use crate::{ReleaseHook, ResError, ResResult};

mod typed;
pub use typed::{CoherentArray, CoherentBox};

/// One resolved contiguous CPU-address to device-bus DMA window.
///
/// Unlike a firmware-local `dma-ranges` entry, both address spaces are fully
/// resolved here: `cpu_start` is a CPU address after all ancestor translation,
/// and `bus_start` is the address the device uses. A view may have exactly one
/// window; disjoint range maps and IOMMU address spaces are not represented and
/// must be rejected by the layer that composes the view.
///
/// Constructors enforce a nonempty interval and reject address-end overflow in
/// either space. Identity mappings must be stated explicitly with
/// [`DmaWindow::IDENTITY`]; absence of firmware data is not identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaWindow {
    cpu_start: u64,
    bus_start: u64,
    last_offset: u64,
}

impl DmaWindow {
    /// The complete 64-bit identity mapping.
    ///
    /// This is an explicit policy statement, not a fallback for missing
    /// firmware ranges.
    pub const IDENTITY: Self = Self {
        cpu_start: 0,
        bus_start: 0,
        last_offset: u64::MAX,
    };

    /// Constructs a resolved window of `len` bytes.
    ///
    /// # Errors
    /// Returns [`ResError::InvalidResource`] for a zero length or when the last
    /// byte overflows either address space.
    pub const fn new(cpu_start: u64, bus_start: u64, len: u64) -> ResResult<Self> {
        let Some(last_offset) = len.checked_sub(1) else {
            return Err(ResError::InvalidResource);
        };
        if cpu_start.checked_add(last_offset).is_none()
            || bus_start.checked_add(last_offset).is_none()
        {
            return Err(ResError::InvalidResource);
        }
        Ok(Self {
            cpu_start,
            bus_start,
            last_offset,
        })
    }

    /// Returns the first CPU address in the window.
    pub const fn cpu_start(self) -> u64 {
        self.cpu_start
    }

    /// Returns the first device bus address in the window.
    pub const fn bus_start(self) -> u64 {
        self.bus_start
    }

    /// Returns whether CPU and device addresses coincide throughout the window.
    pub const fn is_identity(self) -> bool {
        self.cpu_start == self.bus_start
    }

    const fn contains_last_offset(self, last_offset: u64) -> bool {
        last_offset <= self.last_offset
    }
}

/// A device's resolved DMA window, address mask and cache coherency.
///
/// The mask is a device capability and is independent of the window. A
/// firmware range never implies a mask, and a missing property never implies an
/// unlimited or coherent device. [`Self::translate_range`] validates the
/// complete interval against both the window and the mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaInfo {
    mask: u64,
    window: DmaWindow,
    coherent: bool,
}

impl DmaInfo {
    /// Constructs a device DMA view.
    ///
    /// `mask` must be either all-ones or a contiguous set of low address bits
    /// (`DMA_BIT_MASK(n)`). Noncontiguous masks are rejected rather than
    /// rounded. `coherent` is the effective coherency decision; it must come
    /// from device/firmware facts and platform policy, never from a default.
    ///
    /// # Errors
    /// Returns [`ResError::InvalidResource`] for a noncontiguous mask.
    pub const fn new(mask: u64, window: DmaWindow, coherent: bool) -> ResResult<Self> {
        if !valid_mask(mask) {
            return Err(ResError::InvalidResource);
        }
        Ok(Self {
            mask,
            window,
            coherent,
        })
    }

    /// Returns the device address mask.
    pub const fn mask(self) -> u64 {
        self.mask
    }

    /// Returns the resolved CPU-to-device window.
    pub const fn window(self) -> DmaWindow {
        self.window
    }

    /// Returns the effective cache coherency decision.
    pub const fn coherent(self) -> bool {
        self.coherent
    }

    /// Translates one CPU address.
    pub const fn translate(self, cpu_addr: u64) -> Option<u64> {
        self.translate_range(cpu_addr, 1)
    }

    /// Translates a complete nonempty CPU interval, returning its bus base.
    ///
    /// Rejects a zero length, a range outside the window, arithmetic overflow,
    /// and an interval whose last bus byte exceeds the device mask. It performs
    /// no allocation or cache maintenance.
    pub const fn translate_range(self, cpu_start: u64, len: u64) -> Option<u64> {
        if !valid_mask(self.mask) {
            return None;
        }
        let Some(offset) = cpu_start.checked_sub(self.window.cpu_start) else {
            return None;
        };
        let Some(last_byte) = len.checked_sub(1) else {
            return None;
        };
        let Some(last_offset) = offset.checked_add(last_byte) else {
            return None;
        };
        if !self.window.contains_last_offset(last_offset) {
            return None;
        }
        let Some(bus) = self.window.bus_start.checked_add(offset) else {
            return None;
        };
        let Some(last_bus) = self.window.bus_start.checked_add(last_offset) else {
            return None;
        };
        if last_bus > self.mask {
            return None;
        }
        Some(bus)
    }
}

/// Returns whether `mask` is all-ones or a contiguous set of low address bits.
const fn valid_mask(mask: u64) -> bool {
    mask & mask.wrapping_add(1) == 0
}

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug)]
pub struct DmaAllocation {
    /// Virtual address the CPU uses to access the buffer.
    pub cpu_addr: usize,
    /// Bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// The originating allocation request.
    pub spec: DmaSpec,
}

/// A streaming DMA mapping returned by [`DmaOp::map_streaming`].
#[derive(Clone, Copy, Debug)]
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

/// One device-scoped streaming mapping, released exactly once on drop.
///
/// The guard retains the provider's native release action, so the original
/// buffer and any bounce or IOMMU mapping are released together even though the
/// device was given a translated address.
pub struct StreamingMapping {
    bus_addr: u64,
    len: usize,
    release: ReleaseHook,
}

impl fmt::Debug for StreamingMapping {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingMapping")
            .field("bus", &self.bus_addr)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl StreamingMapping {
    /// Creates a guard for one active mapping.
    ///
    /// # Safety
    /// `bus_addr` and `len` must describe a live mapping that `release`
    /// reverses exactly once. `release` must not free the caller-owned buffer
    /// and must obey the provider's documented execution context.
    pub unsafe fn new(bus_addr: u64, len: usize, release: ReleaseHook) -> Self {
        Self {
            bus_addr,
            len,
            release,
        }
    }

    /// Device-visible bus address; zero may be a valid address.
    pub const fn bus_addr(&self) -> u64 {
        self.bus_addr
    }

    /// Mapped length in bytes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mapping has an empty extent.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for StreamingMapping {
    fn drop(&mut self) {
        (self.release)();
    }
}

/// A held capability for one stable device DMA view.
///
/// Every allocation and mapping performed through the capability is constrained
/// by [`Self::info`]. Unlike [`DmaOp`], the capability is device-scoped and must
/// never select a device or window from ambient state. Holding it does not
/// authorize DMA after device removal: the driver stops hardware before
/// releasing its guards.
///
/// The capability is deliberately limited to one continuous window and
/// coherent operation. Hosts that cannot satisfy that policy must return
/// [`ResError::Unsupported`] rather than weakening the view. Streaming mappings
/// return a guard that retains the host's native release action.
pub trait DeviceDma: Send + Sync {
    /// Returns the immutable view applied to every operation.
    fn info(&self) -> DmaInfo;

    /// Allocates a zero-filled coherent buffer that fits the view.
    ///
    /// The complete allocation, including page rounding performed by the host,
    /// must lie inside both the window and the device mask. The returned bytes
    /// are zero for the full requested extent before any device or CPU access;
    /// callers that need stale-free buffers must not depend on the underlying
    /// page allocator happening to zero memory.
    ///
    /// # Errors
    /// Returns [`ResError::InvalidResource`] for an invalid request and
    /// [`ResError::NoMemory`] when no native allocation satisfies the view.
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation>;

    /// Frees a buffer previously returned by [`Self::alloc_coherent`].
    fn free_coherent(&self, alloc: DmaAllocation);

    /// Maps caller-owned memory for streaming DMA.
    ///
    /// The returned guard's bus address is translated through the device view
    /// and its drop releases the native mapping. The caller keeps ownership of
    /// the buffer.
    ///
    /// # Safety
    /// The buffer must contain initialized bytes, remain live and exclusively
    /// held until the guard is dropped, and device access must stop first.
    ///
    /// # Errors
    /// Default: [`ResError::Unsupported`].
    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<StreamingMapping> {
        let _ = (buffer, direction);
        Err(ResError::Unsupported)
    }
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaDirection {
    /// CPU writes, device reads (host flushes before device access).
    DriverToDevice,
    /// Device writes, CPU reads (host invalidates before CPU access).
    DeviceToDriver,
    /// Bidirectional (host flushes and invalidates as needed).
    Bidirectional,
}

unittest::path_mod_test!(coherent_tests, "../ut-tests/coherent.rs");
