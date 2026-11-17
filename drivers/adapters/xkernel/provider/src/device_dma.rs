// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-scoped DMA capability over [`kdma`].
//!
//! The capability owns the resolved [`DmaInfo`] and applies it to every
//! allocation and streaming mapping. Native allocations go through `kdma`, so
//! page flags, platform sharing and release pairing stay in one place. The
//! translated bus address is checked against the whole allocation span before
//! it is returned to the caller, and coherent buffers are zero-filled here
//! because the page allocator does not promise clean memory.

use alloc::sync::Arc;
use core::{alloc::Layout, ptr::NonNull};

use driver_os_api::{
    DeviceDma, DmaAllocation, DmaDirection, DmaInfo, DmaSpec, ReleaseHook, ResError, ResResult,
    StreamingMapping,
};
use khal::mem::v2p;
use memaddr::{PAGE_SIZE_4K, VirtAddr};

use crate::dma::map_dma_direction;

/// One device's DMA capability.
///
/// Allocation and mapping are legal wherever `kdma` is legal: task or ordinary
/// IRQ context, never NMI, and not while holding a lock ordered after the kdma
/// allocator. Streaming guard release runs the same operation as unmapping.
#[derive(Clone, Copy, Debug)]
pub struct XKernelDeviceDma {
    info: DmaInfo,
}

impl XKernelDeviceDma {
    /// Binds one resolved device view.
    ///
    /// # Errors
    /// Returns [`ResError::Unsupported`] for a noncoherent device, because the
    /// native provider does not yet perform the cache maintenance such a
    /// device requires. It never substitutes a coherent policy.
    pub fn new(info: DmaInfo) -> ResResult<Self> {
        if !info.coherent() {
            return Err(ResError::Unsupported);
        }
        Ok(Self { info })
    }
}

impl DeviceDma for XKernelDeviceDma {
    fn info(&self) -> DmaInfo {
        self.info
    }

    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        if spec.len == 0 {
            return Err(ResError::InvalidResource);
        }
        let layout =
            Layout::from_size_align(spec.len, spec.align).map_err(|_| ResError::InvalidResource)?;
        let span = layout
            .size()
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(ResError::NoMemory)?
            / PAGE_SIZE_4K
            * PAGE_SIZE_4K;
        // SAFETY: `layout` describes a nonempty allocation owned by the caller
        // until `free_coherent`; no device receives the buffer before the view
        // has accepted the translated address.
        let native =
            unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
        let cpu_addr = native.cpu_addr.as_ptr() as usize;
        let physical = v2p(VirtAddr::from_usize(cpu_addr)).as_usize() as u64;
        let Some(bus_addr) = self.info.translate_range(physical, span as u64) else {
            // SAFETY: `native` is the live allocation returned above and has
            // never been published to a device.
            unsafe { kdma::deallocate_dma_memory(native, layout) };
            return Err(ResError::NoMemory);
        };
        let align = spec.align as u64;
        if align > 1 && bus_addr & (align - 1) != 0 {
            // SAFETY: as above; the mapping is still unpublished.
            unsafe { kdma::deallocate_dma_memory(native, layout) };
            return Err(ResError::NoMemory);
        }
        // SAFETY: `native` owns `span` bytes starting at `cpu_addr` (the
        // allocation is page-rounded and `span` is that rounded size). The
        // DeviceDma contract promises a zero-filled buffer; kdma's page
        // allocator does not zero pages, so do it explicitly before publishing.
        unsafe { core::ptr::write_bytes(cpu_addr as *mut u8, 0, span) };
        Ok(DmaAllocation {
            cpu_addr,
            bus_addr,
            spec,
        })
    }

    fn free_coherent(&self, alloc: DmaAllocation) {
        let Ok(layout) = Layout::from_size_align(alloc.spec.len, alloc.spec.align) else {
            return;
        };
        let Some(cpu_addr) = NonNull::new(alloc.cpu_addr as *mut u8) else {
            return;
        };
        let native_bus = kdma::p2b(v2p(VirtAddr::from_usize(alloc.cpu_addr)));
        // SAFETY: `alloc` came from this capability's `alloc_coherent` with the
        // same spec, whose non-null CPU address and layout are retained here.
        unsafe {
            kdma::deallocate_dma_memory(
                kdma::DmaInfo {
                    cpu_addr,
                    bus_addr: native_bus,
                },
                layout,
            )
        };
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<StreamingMapping> {
        let len = buffer.len();
        if len == 0 {
            return Err(ResError::InvalidResource);
        }
        let native_direction = map_dma_direction(direction);
        // SAFETY: the caller guarantees an initialized, live and exclusively
        // held buffer for the mapping lifetime.
        let native = unsafe { kdma::map_dma_buffer(buffer, native_direction) }
            .map_err(|_| ResError::NoMemory)?;
        let bounce_cpu = native.cpu_addr.as_ptr() as usize;
        let physical = v2p(VirtAddr::from_usize(bounce_cpu)).as_usize() as u64;
        let Some(bus_addr) = self.info.translate_range(physical, len as u64) else {
            // SAFETY: this exact native mapping is still unpublished; cancel
            // it without a copy-back so a failed DeviceToDriver mapping cannot
            // overwrite the caller's buffer with uninitialized bounce bytes.
            unsafe { kdma::cancel_dma_mapping(native.bus_addr, len) };
            return Err(ResError::NoMemory);
        };

        let native_bus = native.bus_addr;
        let original_cpu = buffer.as_ptr() as *mut u8 as usize;
        let release: ReleaseHook = Arc::new(move || {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: the original buffer pointer was \
                          non-null when the mapping was created and must remain live until this \
                          sole release hook runs"
            )]
            let pointer =
                NonNull::new(original_cpu as *mut u8).expect("streaming DMA buffer was null");
            let buffer = NonNull::slice_from_raw_parts(pointer, len);
            // SAFETY: this hook owns the exact native mapping created above;
            // it runs once and the caller has stopped device access.
            unsafe { kdma::unmap_dma_buffer(native_bus, buffer, native_direction) };
        });
        // SAFETY: `bus_addr` covers the complete mapped interval and `release`
        // reverses exactly the native mapping retained here.
        Ok(unsafe { StreamingMapping::new(bus_addr, len, release) })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/device_dma.rs");
