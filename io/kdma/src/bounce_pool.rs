// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult, BaseAllocator, ByteAllocator, TlsfByteAllocator};
use memaddr::PAGE_SIZE_4K;

use crate::{DMAInfo, DmaBusAddress};

pub(super) const DMA_BOUNCE_POOL_PAGES: usize = 1024;

pub(super) struct BouncePool {
    cpu_addr: usize,
    bus_addr: DmaBusAddress,
    size: usize,
    allocator: TlsfByteAllocator,
}

pub(super) struct BounceMapping {
    pub cpu_addr: usize,
    pub len: usize,
    pub layout: Layout,
}

impl BouncePool {
    pub(super) fn new(dma: DMAInfo, size: usize) -> Self {
        let mut allocator = TlsfByteAllocator::new();
        allocator.init_region(dma.cpu_addr.as_ptr() as usize, size);
        Self {
            cpu_addr: dma.cpu_addr.as_ptr() as usize,
            bus_addr: dma.bus_addr,
            size,
            allocator,
        }
    }

    pub(super) fn allocate(&mut self, len: usize) -> AllocResult<(DMAInfo, Layout)> {
        let layout = bounce_buffer_layout(len)?;
        let cpu_addr = self.allocator.allocate(layout)?;
        let offset = cpu_addr.as_ptr() as usize - self.cpu_addr;
        let bus_addr = DmaBusAddress::new(self.bus_addr.as_u64() + offset as u64);
        Ok((DMAInfo { cpu_addr, bus_addr }, layout))
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

    fn contains_cpu_addr(&self, cpu_addr: usize) -> bool {
        (self.cpu_addr..self.cpu_addr + self.size).contains(&cpu_addr)
    }
}

pub(super) fn bounce_pool_size() -> usize {
    DMA_BOUNCE_POOL_PAGES * PAGE_SIZE_4K
}

fn bounce_buffer_layout(len: usize) -> AllocResult<Layout> {
    Layout::from_size_align(len, 1).map_err(|_| AllocError::InvalidInput)
}
