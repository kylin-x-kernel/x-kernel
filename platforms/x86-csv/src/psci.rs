// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::psci::PsciOp;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use log::{debug, info, warn};

use crate::config::plat::{SHARED_MEM_BASE, SHARED_MEM_SIZE};
const PAGE_SIZE: usize = 0x1000;
const MAX_PAGES: usize = SHARED_MEM_SIZE / PAGE_SIZE;
const BITMAP_SIZE: usize = (MAX_PAGES + 63) / 64;
struct SharedMemAllocator {
    bitmap: [u64; BITMAP_SIZE],
    next_hint: usize,
    allocated_pages: usize,
}
impl SharedMemAllocator {
    const fn new() -> Self {
        Self {
            bitmap: [0; BITMAP_SIZE],
            next_hint: 0,
            allocated_pages: 0,
        }
    }

    fn alloc_pages(&mut self, pages: usize) -> Option<usize> {
        if pages == 0 {
            return None;
        }
        if pages == 1 {
            return self.alloc_single_page();
        }
        self.alloc_contiguous_pages(pages)
    }

    fn alloc_single_page(&mut self) -> Option<usize> {
        for i in self.next_hint..MAX_PAGES {
            if self.is_page_free(i) {
                self.set_page_allocated(i);
                self.next_hint = i + 1;
                self.allocated_pages += 1;
                return Some(SHARED_MEM_BASE + i * PAGE_SIZE);
            }
        }
        for i in 0..self.next_hint {
            if self.is_page_free(i) {
                self.set_page_allocated(i);
                self.next_hint = i + 1;
                self.allocated_pages += 1;
                return Some(SHARED_MEM_BASE + i * PAGE_SIZE);
            }
        }
        None
    }

    fn alloc_contiguous_pages(&mut self, pages: usize) -> Option<usize> {
        let mut start = 0;
        let mut count = 0;
        for i in 0..MAX_PAGES {
            if self.is_page_free(i) {
                if count == 0 {
                    start = i;
                }
                count += 1;
                if count == pages {
                    for j in start..start + pages {
                        self.set_page_allocated(j);
                    }
                    self.allocated_pages += pages;
                    self.next_hint = start + pages;
                    return Some(SHARED_MEM_BASE + start * PAGE_SIZE);
                }
            } else {
                count = 0;
            }
        }
        None
    }

    fn free_pages(&mut self, paddr: usize, pages: usize) {
        if paddr < SHARED_MEM_BASE || paddr >= SHARED_MEM_BASE + SHARED_MEM_SIZE {
            warn!(
                "free_pages: address {:#x} is outside shared memory region",
                paddr
            );
            return;
        }
        let start_page = (paddr - SHARED_MEM_BASE) / PAGE_SIZE;
        for i in 0..pages {
            let page_idx = start_page + i;
            if page_idx < MAX_PAGES {
                self.set_page_free(page_idx);
                self.allocated_pages = self.allocated_pages.saturating_sub(1);
            }
        }
        if start_page < self.next_hint {
            self.next_hint = start_page;
        }
    }

    #[inline]
    fn is_page_free(&self, page_idx: usize) -> bool {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        (self.bitmap[word_idx] & (1u64 << bit_idx)) == 0
    }

    #[inline]
    fn set_page_allocated(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        self.bitmap[word_idx] |= 1u64 << bit_idx;
    }

    #[inline]
    fn set_page_free(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        self.bitmap[word_idx] &= !(1u64 << bit_idx);
    }
}
static SHARED_ALLOCATOR: LazyInit<SpinNoIrq<SharedMemAllocator>> = LazyInit::new();
pub fn init() {
    SHARED_ALLOCATOR.init_once(SpinNoIrq::new(SharedMemAllocator::new()));
    info!(
        "SEV shared memory pool initialized: base={:#x}, size={:#x} ({} pages)",
        SHARED_MEM_BASE, SHARED_MEM_SIZE, MAX_PAGES
    );
}
pub fn alloc_shared_pages(pages: usize) -> Option<usize> {
    let result = SHARED_ALLOCATOR.lock().alloc_pages(pages);
    if result.is_none() {
        warn!(
            "alloc_shared_pages: failed to allocate {} pages from shared memory pool",
            pages
        );
    }
    result
}
pub fn free_shared_pages(paddr: usize, pages: usize) {
    SHARED_ALLOCATOR.lock().free_pages(paddr, pages);
}
pub fn is_shared_memory(paddr: usize) -> bool {
    paddr >= SHARED_MEM_BASE && paddr < SHARED_MEM_BASE + SHARED_MEM_SIZE
}
pub fn shared_memory_range() -> (usize, usize) {
    (SHARED_MEM_BASE, SHARED_MEM_BASE + SHARED_MEM_SIZE)
}
struct PsciImpl;
#[impl_dev_interface]
impl PsciOp for PsciImpl {
    fn dma_share(phys_addr: usize, size: usize) {
        debug!(
            "share_dma_buffer: paddr={:#x}, size={:#x}, in_shared_region={}",
            phys_addr,
            size,
            is_shared_memory(phys_addr)
        );
    }

    fn dma_unshare(phys_addr: usize, size: usize) {
        debug!(
            "unshare_dma_buffer: paddr={:#x}, size={:#x}",
            phys_addr, size
        );
    }
}
