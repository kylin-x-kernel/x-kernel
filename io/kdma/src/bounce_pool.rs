// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult, BaseAllocator, ByteAllocator, TlsfByteAllocator};
use memaddr::PAGE_SIZE_4K;

use crate::{DmaBusAddress, DmaInfo};

pub(super) const DMA_BOUNCE_POOL_PAGES: usize = 1024;

pub(super) struct BouncePool {
    region: Option<BounceRegion>,
    allocator: TlsfByteAllocator,
}

struct BounceRegion {
    cpu_addr: usize,
    bus_addr: DmaBusAddress,
    size: usize,
}

pub(super) struct BounceMapping {
    pub cpu_addr: usize,
    pub len: usize,
    pub layout: Layout,
}

impl BouncePool {
    pub(super) const fn new() -> Self {
        Self {
            region: None,
            allocator: TlsfByteAllocator::new(),
        }
    }

    pub(super) fn is_initialized(&self) -> bool {
        self.region.is_some()
    }

    pub(super) fn initialize(&mut self, dma: DmaInfo, size: usize) {
        assert!(
            self.region.is_none(),
            "DMA bounce pool is already initialized"
        );
        // The multi-KiB TLSF control block already lives in the static DMA
        // allocator. Only register its backing memory here: constructing or
        // moving the control block on an IRQ-exit idle stack can overflow it.
        self.allocator
            .init_region(dma.cpu_addr.as_ptr() as usize, size);
        self.region = Some(BounceRegion {
            cpu_addr: dma.cpu_addr.as_ptr() as usize,
            bus_addr: dma.bus_addr,
            size,
        });
    }

    pub(super) fn allocate(&mut self, len: usize) -> AllocResult<(DmaInfo, Layout)> {
        let region = self.region.as_ref().ok_or(AllocError::InvalidInput)?;
        let layout = bounce_buffer_layout(len)?;
        let cpu_addr = self.allocator.allocate(layout)?;
        let offset = cpu_addr.as_ptr() as usize - region.cpu_addr;
        let bus_addr = DmaBusAddress::new(region.bus_addr.as_u64() + offset as u64);
        Ok((DmaInfo { cpu_addr, bus_addr }, layout))
    }

    pub(super) fn deallocate(&mut self, cpu_addr: usize, layout: Layout) {
        assert!(
            self.contains_cpu_addr(cpu_addr),
            "DMA bounce buffer {:#x} is outside the configured bounce pool",
            cpu_addr
        );
        // SAFETY: `cpu_addr` was validated to belong to this pool and `layout`
        // comes from the matching allocation path.
        unsafe {
            self.allocator
                .deallocate(NonNull::new_unchecked(cpu_addr as *mut u8), layout);
        }
    }

    /// Returns `(used_bytes, total_bytes)` of the bounce allocator.
    #[cfg(unittest)]
    pub(super) fn test_used_total_bytes(&self) -> (usize, usize) {
        (self.allocator.used_bytes(), self.allocator.total_bytes())
    }

    fn contains_cpu_addr(&self, cpu_addr: usize) -> bool {
        self.region.as_ref().is_some_and(|region| {
            (region.cpu_addr..region.cpu_addr + region.size).contains(&cpu_addr)
        })
    }
}

pub(super) fn bounce_pool_size() -> usize {
    DMA_BOUNCE_POOL_PAGES * PAGE_SIZE_4K
}

fn bounce_buffer_layout(len: usize) -> AllocResult<Layout> {
    Layout::from_size_align(len, 1).map_err(|_| AllocError::InvalidInput)
}

#[cfg(unittest)]
unittest::path_mod_test!(mod tests, "../ut-tests/bounce_pool.rs");
