// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult, BaseAllocator, ByteAllocator};
use kalloc::{DefaultByteAllocator, UsageKind, global_allocator};
use khal::{mem::v2p, paging::MappingFlags};
use kspin::SpinNoIrq;
use log::{debug, error};
use memaddr::{PAGE_SIZE_4K, VirtAddr, va};

use crate::{DMAInfo, DmaBusAddress, p2b};

/// Interface for updating page table flags.
/// This breaks the cyclic dependency: kdma -> axmm -> axfs -> axdriver -> kdma
#[crate_interface::def_interface]
pub trait DmaPageTableIf {
    /// Update the mapping flags for the given virtual address range.
    fn protect(vaddr: VirtAddr, size: usize, flags: MappingFlags) -> kerrno::KResult;
}

pub(crate) static ALLOCATOR: SpinNoIrq<DmaAllocator> = SpinNoIrq::new(DmaAllocator::new());

pub(crate) struct DmaAllocator {
    alloc: DefaultByteAllocator,
}

impl DmaAllocator {
    pub const fn new() -> Self {
        Self {
            alloc: DefaultByteAllocator::new(),
        }
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// It firstly tries to allocate from the coherent byte allocator. If there is no
    /// memory, it asks the global page allocator for more memory and adds it to the
    /// byte allocator.
    pub unsafe fn allocate_dma_memory(&mut self, layout: Layout) -> AllocResult<DMAInfo> {
        if layout.size() >= PAGE_SIZE_4K {
            self.alloc_coherent_pages(layout)
        } else {
            self.alloc_coherent_bytes(layout)
        }
    }

    fn alloc_coherent_bytes(&mut self, layout: Layout) -> AllocResult<DMAInfo> {
        let mut is_expanded = false;
        loop {
            if let Ok(data) = self.alloc.allocate(layout) {
                let cpu_addr = va!(data.as_ptr() as usize);
                return Ok(DMAInfo {
                    cpu_addr: data,
                    bus_addr: v2b(cpu_addr),
                });
            } else {
                if is_expanded {
                    return Err(AllocError::NoMemory);
                }
                is_expanded = true;
                let available_pages = global_allocator().available_pages();
                // 4 pages or available pages.
                let num_pages = 4.min(available_pages);
                let expand_size = num_pages * PAGE_SIZE_4K;
                let vaddr_raw =
                    global_allocator().alloc_pages(num_pages, PAGE_SIZE_4K, UsageKind::Dma)?;
                let vaddr = va!(vaddr_raw);
                let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::UNCACHED;
                #[cfg(feature = "sev")]
                // For SEV, DMA memory must be shared (not encrypted)
                let flags = flags | MappingFlags::SHARED;
                self.update_flags(vaddr, num_pages, flags)?;
                self.alloc
                    .add_region(vaddr_raw, expand_size)
                    .inspect_err(|e| error!("add memory fail: {e:?}"))?;
                debug!("expand memory @{vaddr:#X}, size: {expand_size:#X} bytes");
            }
        }
    }

    fn alloc_coherent_pages(&mut self, layout: Layout) -> AllocResult<DMAInfo> {
        let num_pages = layout_pages(&layout);
        let vaddr_raw = global_allocator().alloc_pages(
            num_pages,
            PAGE_SIZE_4K.max(layout.align()),
            UsageKind::Dma,
        )?;
        let vaddr = va!(vaddr_raw);
        let flags = MappingFlags::READ | MappingFlags::WRITE | MappingFlags::UNCACHED;
        #[cfg(feature = "sev")]
        // For SEV, DMA memory must be shared (not encrypted)
        let flags = flags | MappingFlags::SHARED;
        self.update_flags(vaddr, num_pages, flags)?;
        Ok(DMAInfo {
            cpu_addr: unsafe { NonNull::new_unchecked(vaddr_raw as *mut u8) },
            bus_addr: v2b(vaddr),
        })
    }

    fn update_flags(
        &mut self,
        vaddr: VirtAddr,
        num_pages: usize,
        flags: MappingFlags,
    ) -> AllocResult<()> {
        let expand_size = num_pages * PAGE_SIZE_4K;
        crate_interface::call_interface!(DmaPageTableIf::protect(vaddr, expand_size, flags))
            .map_err(|_| {
                error!("change table flag fail");
                AllocError::NoMemory
            })
    }

    /// Gives back the allocated region to the byte allocator.
    pub unsafe fn deallocate_dma_memory(&mut self, dma: DMAInfo, layout: Layout) {
        if layout.size() >= PAGE_SIZE_4K {
            let num_pages = layout_pages(&layout);
            let virt_raw = dma.cpu_addr.as_ptr() as usize;
            use core::sync::atomic::{Ordering, fence};

            let size = num_pages * PAGE_SIZE_4K;
            let vaddr = virt_raw as *mut u8;

            unsafe {
                core::ptr::write_bytes(vaddr, 0, size);
            }
            fence(Ordering::SeqCst);

            let _ = self.update_flags(
                va!(virt_raw),
                num_pages,
                MappingFlags::READ | MappingFlags::WRITE | MappingFlags::UNCACHED,
            );
            fence(Ordering::SeqCst);

            global_allocator().dealloc_pages(virt_raw, num_pages, UsageKind::Dma);
        } else {
            self.alloc.deallocate(dma.cpu_addr, layout)
        }
    }
}

fn v2b(addr: VirtAddr) -> DmaBusAddress {
    let paddr = v2p(addr);
    p2b(paddr)
}

const fn layout_pages(layout: &Layout) -> usize {
    memaddr::align_up_4k(layout.size()) / PAGE_SIZE_4K
}
