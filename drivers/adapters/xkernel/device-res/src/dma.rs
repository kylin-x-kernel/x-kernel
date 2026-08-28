// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`device_res::DmaOp`] provider contract.

use core::{alloc::Layout, ptr::NonNull};

use device_res::{DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec, ResError, ResResult};

use crate::XKernelResourceProvider;

impl DmaOp for XKernelResourceProvider {
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        let layout =
            Layout::from_size_align(spec.len, spec.align).map_err(|_| ResError::InvalidResource)?;
        // SAFETY: `layout` is a valid non-zero layout and the returned buffer is
        // owned exclusively by the `DmaCoherent` handle that wraps this
        // allocation until it is freed via `free_coherent`.
        let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
        Ok(DmaAllocation {
            cpu_addr: info.cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            spec,
        })
    }

    fn free_coherent(&self, alloc: DmaAllocation) {
        let Ok(layout) = Layout::from_size_align(alloc.spec.len, alloc.spec.align) else {
            return;
        };
        let info = kdma::DMAInfo {
            cpu_addr: NonNull::new(alloc.cpu_addr as *mut u8)
                .expect("coherent DMA allocation stored a null CPU address"),
            bus_addr: kdma::DmaBusAddress::new(alloc.bus_addr),
        };
        // SAFETY: `info` and `layout` describe a coherent buffer previously
        // returned by `alloc_coherent` for the same spec, and it is freed
        // exactly once when its owning handle is dropped.
        unsafe { kdma::deallocate_dma_memory(info, layout) };
    }

    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let dir = map_dma_direction(direction);
        // SAFETY: the caller guarantees `buffer` is a valid, live slice for the
        // duration of the mapping.
        let slice: &[u8] = unsafe { buffer.as_ref() };
        let len = slice.len();
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        // SAFETY: same contract - `buffer` is valid for the mapping duration.
        let info = unsafe { kdma::map_dma_buffer(buffer, dir) }.map_err(|_| ResError::NoMemory)?;
        Ok(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            len,
            direction,
        })
    }

    fn unmap_streaming(&self, mapping: DmaMapping) {
        if mapping.bus_addr == 0 {
            return;
        }

        let cpu_addr = NonNull::new(mapping.cpu_addr as *mut u8)
            .expect("streaming DMA mapping stored a null CPU address");
        let buffer = NonNull::slice_from_raw_parts(cpu_addr, mapping.len);
        // SAFETY: `mapping` describes a streaming mapping previously established
        // by `map_streaming`; `cpu_addr` + `len` reconstruct the original buffer.
        unsafe {
            kdma::unmap_dma_buffer(
                kdma::DmaBusAddress::new(mapping.bus_addr),
                buffer,
                map_dma_direction(mapping.direction),
            );
        }
    }
}

/// Translate a device-res DMA direction into the matching `kdma` direction.
fn map_dma_direction(d: DmaDirection) -> kdma::DmaDirection {
    match d {
        DmaDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional => kdma::DmaDirection::Bidirectional,
    }
}
