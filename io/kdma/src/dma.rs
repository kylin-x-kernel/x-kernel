// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::btree_map::BTreeMap, vec::Vec};
#[cfg(feature = "dma-trace")]
use core::panic::Location;
use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult};
use kalloc::{UsageKind, global_allocator};
use khal::mem::v2p;
use kiface_mm_api::{DmaMappingCompletion, DmaMappingIf, DmaMappingMode};
use kspin::{SpinNoIrq, static_lock};
use log::error;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, va};

use crate::{
    DmaBusAddress, DmaDirection, DmaInfo,
    bounce_pool::{BounceMapping, BouncePool, DMA_BOUNCE_POOL_PAGES, bounce_pool_size},
    p2b,
};

static_lock! {
    pub(crate) static ALLOCATOR: SpinNoIrq<DmaAllocator> = SpinNoIrq::new(DmaAllocator::new());
}

pub(crate) struct DmaAllocator {
    /// Detached kernel invalidations produced by mode changes while this
    /// allocator was locked. The public wrappers drain them after unlocking.
    pending_completions: Vec<DmaMappingCompletion>,
    bounce_pool: BouncePool,
    active_mappings: BTreeMap<u64, BounceMapping>,
    #[cfg(feature = "dma-trace")]
    coherent_allocs: BTreeMap<usize, CoherentAllocation>,
    /// Test-only in-use coherent allocations, keyed by CPU address.
    #[cfg(unittest)]
    test_coherent_allocs: BTreeMap<usize, usize>,
}

/// Read-only snapshot of in-use DMA resources, for unit-test baselines.
#[cfg(unittest)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DmaTestStats {
    /// Number of active streaming (bounce) mappings.
    pub active_mappings: usize,
    /// Total bytes of active streaming (bounce) mappings.
    pub active_mapping_bytes: usize,
    /// Bounce-pool bytes currently allocated.
    pub bounce_used_bytes: usize,
    /// Bounce-pool total size, or zero before it is initialized.
    pub bounce_total_bytes: usize,
    /// Number of active coherent allocations.
    pub coherent_allocations: usize,
    /// Total bytes of active coherent allocations.
    pub coherent_bytes: usize,
}

#[cfg(feature = "dma-trace")]
#[derive(Clone, Copy)]
struct TraceSite {
    file: &'static str,
    line: u32,
    column: u32,
}

#[cfg(feature = "dma-trace")]
#[derive(Clone, Copy)]
struct CoherentAllocation {
    bus_addr: DmaBusAddress,
    num_pages: usize,
    site: TraceSite,
}

impl DmaAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            pending_completions: Vec::new(),
            bounce_pool: BouncePool::new(),
            active_mappings: BTreeMap::new(),
            #[cfg(feature = "dma-trace")]
            coherent_allocs: BTreeMap::new(),
            #[cfg(unittest)]
            test_coherent_allocs: BTreeMap::new(),
        }
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// DMA memory is allocated in page granularity so page-table attributes and
    /// platform share/unshare hooks remain balanced for every allocation.
    ///
    /// # Safety
    ///
    /// The caller must treat the returned [`DmaInfo`] as coherent DMA memory:
    /// deallocate it exactly once with the same `layout`, do not forge or
    /// offset the returned CPU address, and only hand the bus address to a
    /// device that is allowed to DMA that range.
    #[track_caller]
    pub(crate) unsafe fn allocate_dma_memory(&mut self, layout: Layout) -> AllocResult<DmaInfo> {
        self.alloc_coherent_pages(layout)
    }

    #[track_caller]
    fn alloc_coherent_pages(&mut self, layout: Layout) -> AllocResult<DmaInfo> {
        let num_pages = layout_pages(&layout);
        let vaddr_raw = global_allocator().alloc_dma_pages(
            num_pages,
            PAGE_SIZE_4K.max(layout.align()),
            UsageKind::Dma,
        )?;
        let vaddr = va!(vaddr_raw);
        if let Some(completion) =
            self.update_mapping_mode(vaddr, num_pages, DmaMappingMode::Coherent)?
        {
            self.pending_completions.push(completion);
        }
        self.prepare_platform_dma(v2p(vaddr), num_pages * PAGE_SIZE_4K)?;
        let dma_info = DmaInfo {
            // SAFETY: `vaddr_raw` is the start of the freshly allocated DMA mapping
            // and therefore cannot be null.
            cpu_addr: unsafe { NonNull::new_unchecked(vaddr_raw as *mut u8) },
            bus_addr: v2b(vaddr),
        };
        self.trace_coherent_alloc(dma_info, num_pages);
        #[cfg(unittest)]
        self.test_coherent_allocs.insert(
            dma_info.cpu_addr.as_ptr() as usize,
            num_pages * PAGE_SIZE_4K,
        );
        Ok(dma_info)
    }

    /// Applies one mapping mode and returns the invalidation the provider
    /// detached from its lock, if it did not complete it in place.
    fn update_mapping_mode(
        &mut self,
        vaddr: VirtAddr,
        num_pages: usize,
        mode: DmaMappingMode,
    ) -> AllocResult<Option<DmaMappingCompletion>> {
        let expand_size = num_pages * PAGE_SIZE_4K;
        DmaMappingIf::set_mode(vaddr, expand_size, mode).map_err(|_| {
            error!("change table flag fail");
            AllocError::NoMemory
        })
    }

    /// Takes the detached invalidations accumulated under this lock.
    pub(crate) fn take_completions(&mut self) -> Vec<DmaMappingCompletion> {
        core::mem::take(&mut self.pending_completions)
    }

    fn prepare_platform_dma(&mut self, paddr: PhysAddr, size: usize) -> AllocResult<()> {
        kplat::dma::prepare(paddr.as_usize(), size).map_err(|_| {
            error!("platform dma prepare failed");
            AllocError::NoMemory
        })
    }

    fn release_platform_dma(&mut self, paddr: PhysAddr, size: usize) -> AllocResult<()> {
        kplat::dma::release(paddr.as_usize(), size).map_err(|_| {
            error!("platform dma release failed");
            AllocError::NoMemory
        })
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// # Safety
    ///
    /// `dma` must come from a prior successful [`allocate_dma_memory`] call on
    /// this allocator, and `layout` must exactly match that allocation.
    #[track_caller]
    pub(crate) unsafe fn deallocate_dma_memory(&mut self, dma: DmaInfo, layout: Layout) {
        let num_pages = layout_pages(&layout);
        if !self.trace_coherent_free(dma, num_pages) {
            return;
        }
        #[cfg(unittest)]
        self.test_coherent_allocs
            .remove(&(dma.cpu_addr.as_ptr() as usize));
        let virt_raw = dma.cpu_addr.as_ptr() as usize;
        let size = num_pages * PAGE_SIZE_4K;
        let vaddr = va!(virt_raw);

        // Deallocation cannot fail; the helper already logs platform release errors.
        let _ = self.release_platform_dma(v2p(vaddr), size);
        // The pages may return to the general allocator only after every CPU
        // has dropped its uncached translation: otherwise a CPU that still
        // holds the coherent mapping could observe a cached alias of the
        // reused page. The provider detaches that invalidation from the
        // allocator lock, so release the pages inside the completion, which
        // the public wrapper runs after unlocking.
        match self.update_mapping_mode(vaddr, num_pages, DmaMappingMode::Normal) {
            Ok(Some(flush)) => {
                self.pending_completions
                    .push(DmaMappingCompletion::new(move || {
                        let is_invalidated = flush.complete();
                        global_allocator().dealloc_dma_pages(virt_raw, num_pages, UsageKind::Dma);
                        is_invalidated
                    }));
            }
            // The provider completed the invalidation in place, or the mode
            // restore failed after logging; no flush is pending.
            Ok(None) | Err(_) => {
                global_allocator().dealloc_dma_pages(virt_raw, num_pages, UsageKind::Dma);
            }
        }
    }

    /// Returns a read-only snapshot of in-use DMA resources for test baselines.
    #[cfg(unittest)]
    pub(crate) fn test_stats(&self) -> DmaTestStats {
        let (bounce_used_bytes, bounce_total_bytes) = self.bounce_pool.test_used_total_bytes();
        DmaTestStats {
            active_mappings: self.active_mappings.len(),
            active_mapping_bytes: self.active_mappings.values().map(|m| m.len).sum(),
            bounce_used_bytes,
            bounce_total_bytes,
            coherent_allocations: self.test_coherent_allocs.len(),
            coherent_bytes: self.test_coherent_allocs.values().sum(),
        }
    }

    /// Maps an existing CPU buffer through the bounce pool for a temporary DMA transaction.
    ///
    /// # Safety
    ///
    /// `buffer` must remain live and exclusively owned by the caller until the
    /// matching [`unmap_dma_buffer`] call. The caller must not let the device
    /// access the returned bus address after unmapping.
    pub(crate) unsafe fn map_dma_buffer(
        &mut self,
        buffer: NonNull<[u8]>,
        _direction: DmaDirection,
    ) -> AllocResult<DmaInfo> {
        use core::sync::atomic::{Ordering, fence};

        let len = buffer.len();
        if len == 0 {
            return Err(AllocError::InvalidInput);
        }

        fence(Ordering::SeqCst);
        let (dma_info, layout) = self.alloc_bounce_buffer(len)?;

        // Stage the current bytes for every direction, including DeviceToDriver.
        // The device's writes still overwrite them, but a mapping that is
        // released without device access (queue-setup rollback) then returns
        // exactly the original data instead of copied-back pool contents.
        // SAFETY: the bounce buffer and source slice are distinct live regions
        // of `len` bytes established by the caller and allocator.
        unsafe {
            core::ptr::copy_nonoverlapping(
                buffer.as_ptr() as *const u8,
                dma_info.cpu_addr.as_ptr(),
                len,
            );
        }
        fence(Ordering::SeqCst);

        if self
            .active_mappings
            .contains_key(&dma_info.bus_addr.as_u64())
        {
            error!("DMA mapping is already active: {:?}", dma_info.bus_addr);
            self.recycle_bounce_buffer(dma_info.cpu_addr.as_ptr() as usize, layout);
            return Err(AllocError::InvalidInput);
        }
        self.active_mappings.insert(
            dma_info.bus_addr.as_u64(),
            BounceMapping {
                cpu_addr: dma_info.cpu_addr.as_ptr() as usize,
                len,
                layout,
            },
        );

        Ok(dma_info)
    }

    /// Releases a bounced mapping that the device never received.
    ///
    /// The mapping must be unpublished: no device may have accessed its bus
    /// address. Unlike [`Self::unmap_dma_buffer`] this performs no copy-back,
    /// so rollback cannot overwrite the original buffer with stale or
    /// uninitialized bounce contents.
    ///
    /// # Safety
    ///
    /// `dma_addr` must be the still-active mapping returned by
    /// [`Self::map_dma_buffer`] and must be cancelled exactly once. The device
    /// must never have been given the address.
    pub(crate) unsafe fn cancel_dma_mapping(&mut self, dma_addr: DmaBusAddress, len: usize) {
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: The Safety contract requires dma_addr \
                      to be the still-active mapping from map_dma_buffer, cancelled exactly once"
        )]
        let mapping = self
            .active_mappings
            .remove(&dma_addr.as_u64())
            .unwrap_or_else(|| panic!("DMA mapping is not active: {:?}", dma_addr));

        assert_eq!(
            len, mapping.len,
            "DMA buffer length mismatch for {:?}: expected {}, got {}",
            dma_addr, mapping.len, len
        );

        self.recycle_bounce_buffer(mapping.cpu_addr, mapping.layout);
    }

    /// Unmaps a previously bounced DMA buffer and synchronizes data back.
    ///
    /// # Safety
    ///
    /// `dma_addr` must be the still-active mapping returned by
    /// [`map_dma_buffer`] for the same `buffer`, and this function must be
    /// called exactly once for that mapping.
    pub(crate) unsafe fn unmap_dma_buffer(
        &mut self,
        dma_addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) {
        use core::sync::atomic::{Ordering, fence};

        let len = buffer.len();
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: The Safety contract requires dma_addr \
                      to be the still-active mapping from map_dma_buffer, unmapped exactly once"
        )]
        let mapping = self
            .active_mappings
            .remove(&dma_addr.as_u64())
            .unwrap_or_else(|| panic!("DMA mapping is not active: {:?}", dma_addr));

        assert_eq!(
            len, mapping.len,
            "DMA buffer length mismatch for {:?}: expected {}, got {}",
            dma_addr, mapping.len, len
        );

        fence(Ordering::SeqCst);
        if direction != DmaDirection::DriverToDevice {
            // SAFETY: the bounce buffer and destination slice are distinct live
            // regions of `len` bytes tracked by the active mapping record.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    mapping.cpu_addr as *const u8,
                    buffer.as_ptr() as *mut u8,
                    len,
                );
            }
            fence(Ordering::SeqCst);
        }

        self.recycle_bounce_buffer(mapping.cpu_addr, mapping.layout);
    }

    fn alloc_bounce_buffer(&mut self, len: usize) -> AllocResult<(DmaInfo, Layout)> {
        let pool = self.bounce_pool_mut()?;
        pool.allocate(len)
    }

    fn recycle_bounce_buffer(&mut self, cpu_addr: usize, layout: Layout) {
        self.bounce_pool.deallocate(cpu_addr, layout);
    }

    fn bounce_pool_mut(&mut self) -> AllocResult<&mut BouncePool> {
        if !self.bounce_pool.is_initialized() {
            let dma = self.alloc_coherent_pages(layout_for_pages(DMA_BOUNCE_POOL_PAGES))?;
            self.bounce_pool.initialize(dma, bounce_pool_size());
        }
        Ok(&mut self.bounce_pool)
    }

    #[cfg(feature = "dma-trace")]
    #[track_caller]
    fn trace_coherent_alloc(&mut self, dma: DmaInfo, num_pages: usize) {
        let cpu_addr = dma.cpu_addr.as_ptr() as usize;
        let site = TraceSite::caller();
        if let Some(prev) = self.coherent_allocs.get(&cpu_addr).copied() {
            error!(
                "duplicate coherent DMA allocation record: cpu_addr={:#x}, bus_addr={:#x}, \
                 pages={}, alloc_site={} while previous record bus_addr={:#x}, pages={}, \
                 alloc_site={}",
                cpu_addr,
                dma.bus_addr.as_u64(),
                num_pages,
                site,
                prev.bus_addr.as_u64(),
                prev.num_pages,
                prev.site
            );
            return;
        }
        self.coherent_allocs.insert(
            cpu_addr,
            CoherentAllocation {
                bus_addr: dma.bus_addr,
                num_pages,
                site,
            },
        );
    }

    #[cfg(not(feature = "dma-trace"))]
    fn trace_coherent_alloc(&mut self, _dma: DmaInfo, _num_pages: usize) {}

    #[cfg(feature = "dma-trace")]
    #[track_caller]
    fn trace_coherent_free(&mut self, dma: DmaInfo, num_pages: usize) -> bool {
        let cpu_addr = dma.cpu_addr.as_ptr() as usize;
        let site = TraceSite::caller();

        if let Some(record) = self.coherent_allocs.get(&cpu_addr).copied() {
            if record.bus_addr != dma.bus_addr || record.num_pages != num_pages {
                error!(
                    "coherent DMA free mismatch: cpu_addr={:#x}, bus_addr={:#x}, pages={}, \
                     free_site={}, expected bus_addr={:#x}, pages={}, alloc_site={}; skipping \
                     deallocation",
                    cpu_addr,
                    dma.bus_addr.as_u64(),
                    num_pages,
                    site,
                    record.bus_addr.as_u64(),
                    record.num_pages,
                    record.site
                );
                return false;
            }
            self.coherent_allocs.remove(&cpu_addr);
            return true;
        }

        if let Some((expected_cpu_addr, record)) =
            self.coherent_allocs
                .iter()
                .find_map(|(tracked_cpu_addr, record)| {
                    (record.bus_addr == dma.bus_addr).then_some((*tracked_cpu_addr, *record))
                })
        {
            error!(
                "coherent DMA free used unexpected cpu_addr: cpu_addr={:#x}, bus_addr={:#x}, \
                 pages={}, free_site={}, expected cpu_addr={:#x}, pages={}, alloc_site={}; \
                 skipping deallocation",
                cpu_addr,
                dma.bus_addr.as_u64(),
                num_pages,
                site,
                expected_cpu_addr,
                record.num_pages,
                record.site
            );
            return false;
        }

        error!(
            "coherent DMA free for untracked allocation: cpu_addr={:#x}, bus_addr={:#x}, \
             pages={}, free_site={}; skipping deallocation",
            cpu_addr,
            dma.bus_addr.as_u64(),
            num_pages,
            site
        );
        false
    }

    #[cfg(not(feature = "dma-trace"))]
    fn trace_coherent_free(&mut self, _dma: DmaInfo, _num_pages: usize) -> bool {
        true
    }
}

#[cfg(feature = "dma-trace")]
impl TraceSite {
    #[track_caller]
    fn caller() -> Self {
        let location = Location::caller();
        Self {
            file: location.file(),
            line: location.line(),
            column: location.column(),
        }
    }
}

#[cfg(feature = "dma-trace")]
impl core::fmt::Display for TraceSite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

fn v2b(addr: VirtAddr) -> DmaBusAddress {
    let paddr = v2p(addr);
    p2b(paddr)
}

const fn layout_pages(layout: &Layout) -> usize {
    memaddr::align_up_4k(layout.size()) / PAGE_SIZE_4K
}

fn layout_for_pages(num_pages: usize) -> Layout {
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: the only caller passes the fixed \
                  DMA_BOUNCE_POOL_PAGES constant; its byte size fits Layout and PAGE_SIZE_4K is a \
                  power of two"
    )]
    Layout::from_size_align(num_pages * PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap()
}
