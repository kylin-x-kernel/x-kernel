// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel global allocator and page allocation helpers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

#[cfg(any(feature = "dice", feature = "tee"))]
mod ffi;

use core::{
    alloc::{GlobalAlloc, Layout},
    fmt,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[allow(unused_imports)]
use alloc_engine::{
    AllocError, AllocResult, BaseAllocator, BuddyPageAllocator, ByteAllocator, PageAllocator,
};
use kaddr_layout::v2p;
use kspin::{IrqSave, NoPreempt, SpinNoIrq};
use memaddr::PAGE_SIZE_4K;
use strum::{IntoStaticStr, VariantArray};
const MIN_HEAP_SIZE: usize = 0x8000; // 32 K

/// Keep medium-sized buffers in the byte heap so they can reuse memory already
/// transferred from the page allocator instead of requiring a new high-order block.
const LARGE_ALLOC_THRESHOLD_BYTES: usize = 1024 * 1024;

fn pages_to_bytes(num_pages: usize) -> usize {
    num_pages
        .checked_mul(PAGE_SIZE_4K)
        .expect("page count byte size overflow")
}

fn kernel_virt_to_phys(vaddr: usize) -> usize {
    v2p(vaddr)
}

mod page;
mod pcp;
#[cfg(feature = "slab")]
mod slab_cache;
pub use page::GlobalPage;

#[cfg(feature = "tracking")]
mod tracking;
#[cfg(feature = "tracking")]
pub use tracking::*;

// Exactly one allocator feature must be selected. Kernel builds get it
// through kfeat; host test builds pass it explicitly (e.g.
// `cargo test -p kext4 --features kalloc/slab`).
#[cfg(not(any(feature = "slab", feature = "buddy", feature = "tlsf")))]
compile_error!("kalloc requires one of the allocator features: slab, buddy, or tlsf");

cfg_if::cfg_if! {
    if #[cfg(feature = "slab")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::SlabByteAllocator;
    } else if #[cfg(feature = "buddy")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::BuddyByteAllocator;
    } else if #[cfg(feature = "tlsf")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::TlsfByteAllocator;
    }
}

/// Kinds of memory usage for tracking.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray, IntoStaticStr)]
pub enum UsageKind {
    /// Heap allocations made by kernel Rust code.
    RustHeap,
    /// Virtual memory, usually used for user space.
    VirtMem,
    /// Page cache for file systems.
    PageCache,
    /// Page tables.
    PageTable,
    /// DMA memory.
    Dma,
    /// Memory used by [`GlobalPage`].
    Global,
}

/// Statistics of memory usage by category.
#[derive(Clone, Copy)]
pub struct Usages([usize; UsageKind::VARIANTS.len()]);

impl Usages {
    /// Creates a zero-initialized usage snapshot.
    const fn new() -> Self {
        Self([0; UsageKind::VARIANTS.len()])
    }

    /// Return usage in bytes for the given kind.
    pub fn get(&self, kind: UsageKind) -> usize {
        self.0[kind as usize]
    }
}

/// Per-CPU cumulative counters from which [`Usages`] snapshots are collected.
struct UsageCounters {
    allocated_bytes: [AtomicUsize; UsageKind::VARIANTS.len()],
    freed_bytes: [AtomicUsize; UsageKind::VARIANTS.len()],
}

impl UsageCounters {
    const fn new() -> Self {
        Self {
            allocated_bytes: [const { AtomicUsize::new(0) }; UsageKind::VARIANTS.len()],
            freed_bytes: [const { AtomicUsize::new(0) }; UsageKind::VARIANTS.len()],
        }
    }

    fn alloc(&self, kind: UsageKind, size_bytes: usize) {
        self.allocated_bytes[kind as usize]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(size_bytes)
            })
            .expect("kalloc usage overflow");
    }

    fn dealloc(&self, kind: UsageKind, size_bytes: usize) {
        // Release publishes the allocation that must precede every valid
        // free. The snapshot acquires freed totals before reading allocations.
        self.freed_bytes[kind as usize]
            .fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
                current.checked_add(size_bytes)
            })
            .expect("kalloc freed usage overflow");
    }
}

struct UsageTotals {
    allocated_bytes: [u128; UsageKind::VARIANTS.len()],
    freed_bytes: [u128; UsageKind::VARIANTS.len()],
}

impl UsageTotals {
    const fn new() -> Self {
        Self {
            allocated_bytes: [0; UsageKind::VARIANTS.len()],
            freed_bytes: [0; UsageKind::VARIANTS.len()],
        }
    }

    fn collect_freed(&mut self, counters: &UsageCounters) {
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            self.freed_bytes[index] = self.freed_bytes[index]
                .checked_add(counters.freed_bytes[index].load(Ordering::Acquire) as u128)
                .expect("kalloc freed usage total overflow");
        }
    }

    fn collect_allocated(&mut self, counters: &UsageCounters) {
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            self.allocated_bytes[index] = self.allocated_bytes[index]
                .checked_add(counters.allocated_bytes[index].load(Ordering::Relaxed) as u128)
                .expect("kalloc allocated usage total overflow");
        }
    }

    fn finish(self) -> Usages {
        let mut usages = Usages::new();
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            let live_bytes = self.allocated_bytes[index]
                .checked_sub(self.freed_bytes[index])
                .expect("kalloc usage underflow");
            usages.0[index] = usize::try_from(live_bytes).expect("kalloc usage overflow");
        }
        usages
    }
}

#[percpu::def_percpu]
static PERCPU_USAGE_COUNTERS: UsageCounters = UsageCounters::new();

fn with_current_usage_counters<T>(f: impl FnOnce(&UsageCounters) -> T) -> T {
    let _preempt_guard = NoPreempt::new();
    // SAFETY: the guard pins this execution path while resolving the current
    // CPU slot. Only shared references are created, and every field is atomic,
    // so IRQ re-entry and remote snapshot readers cannot cause a data race.
    let counters = unsafe { PERCPU_USAGE_COUNTERS.current_ref_raw() };
    f(counters)
}

fn usage_snapshot() -> Usages {
    let mut totals = UsageTotals::new();
    let cpu_count = percpu::percpu_area_num();

    // Read every freed counter first. Acquiring a recorded free also observes
    // its causally preceding allocation before the second pass, which prevents
    // a valid cross-CPU free from producing a spurious aggregate underflow.
    for cpu_index in 0..cpu_count {
        // SAFETY: `cpu_index` is bounded by the number of initialized per-CPU
        // areas. Writers and this reader use only the slot's atomic fields.
        let counters = unsafe { PERCPU_USAGE_COUNTERS.remote_ref_raw(cpu_index) };
        totals.collect_freed(counters);
    }
    for cpu_index in 0..cpu_count {
        // SAFETY: same bounds and atomic-access argument as the first pass.
        let counters = unsafe { PERCPU_USAGE_COUNTERS.remote_ref_raw(cpu_index) };
        totals.collect_allocated(counters);
    }

    totals.finish()
}

impl fmt::Debug for Usages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("UsageStats");
        for &kind in UsageKind::VARIANTS {
            d.field(kind.into(), &self.0[kind as usize]);
        }
        d.finish()
    }
}

/// The global allocator used by x-kernel.
///
/// It combines a [`ByteAllocator`] and a [`PageAllocator`] into a simple
/// two-level allocator: firstly tries allocate from the byte allocator, if
/// there is no memory, asks the page allocator for more memory and adds it to
/// the byte allocator.
///
/// The configured byte allocator serves sub-page requests, while
/// [`BuddyPageAllocator`] provides backing pages and direct large allocations.
/// Slab builds add a per-CPU object cache in front of the byte allocator.
pub struct GlobalAllocator {
    balloc: SpinNoIrq<DefaultByteAllocator>,
    /// Whether the byte allocator already owns a bootstrap heap region.
    heap_ready: AtomicBool,
    /// Whether the page allocator has at least one usable memory region.
    page_ready: AtomicBool,
    palloc: SpinNoIrq<BuddyPageAllocator<{ PAGE_SIZE_4K }>>,
}

impl Default for GlobalAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalAllocator {
    /// Creates an empty [`GlobalAllocator`].
    pub const fn new() -> Self {
        Self {
            balloc: SpinNoIrq::new(DefaultByteAllocator::new()),
            heap_ready: AtomicBool::new(false),
            page_ready: AtomicBool::new(false),
            palloc: SpinNoIrq::new(BuddyPageAllocator::new()),
        }
    }

    /// Returns the name of the allocator.
    pub const fn name(&self) -> &'static str {
        cfg_if::cfg_if! {
            if #[cfg(feature = "slab")] {
                "slab"
            } else if #[cfg(feature = "buddy")] {
                "buddy"
            } else if #[cfg(feature = "tlsf")] {
                "TLSF"
            }
        }
    }

    /// Initializes the allocator with the given region.
    ///
    /// This is the legacy "initialize both page allocator and byte allocator"
    /// entry used by paths that already have a stable runtime memory region.
    pub fn init(&self, va: usize, size: usize) {
        assert!(size > MIN_HEAP_SIZE);
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
        self.bootstrap_heap_if_needed()
            .expect("failed to initialize heap memory");
    }

    /// Initialize or extend only the backing page allocator.
    ///
    /// The byte allocator is bootstrapped lazily when memory is later added or
    /// a byte allocation needs it. `va..va + size` must be a mapped, writable,
    /// exclusive region that does not overlap an existing allocator region.
    pub fn init_page_allocator(&self, va: usize, size: usize) {
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
    }

    /// Return whether the backing page allocator has a usable memory region.
    pub fn is_page_allocator_ready(&self) -> bool {
        self.page_ready.load(Ordering::Acquire)
    }

    /// Add the given region to the allocator.
    ///
    /// By default new memory first becomes available to the page allocator.
    pub fn add_memory(&self, va: usize, size: usize) -> AllocResult {
        self.init_or_extend_page_allocator(va, size)?;

        if !self.heap_ready.load(Ordering::Acquire) {
            self.bootstrap_heap_if_needed()?;
        }
        Ok(())
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// It firstly tries to allocate from the byte allocator. If there is no
    /// memory, it asks the page allocator for more memory and adds it to the
    /// byte allocator.
    ///
    /// Large allocations (over `LARGE_ALLOC_THRESHOLD_BYTES`) bypass the byte allocator
    /// to avoid fragmenting the slab heap and are served directly from the
    /// page allocator.
    pub fn alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        // Large allocations: go directly to the page allocator.
        if layout.size() > LARGE_ALLOC_THRESHOLD_BYTES {
            return self.large_alloc(layout);
        }

        #[cfg(feature = "slab")]
        if let Some(size_class) = alloc_engine::SizeClass::from_layout(layout) {
            let object = self.alloc_from_percpu_slab(size_class)?;
            self.record_usage_alloc(UsageKind::RustHeap, layout.size());
            return Ok(object);
        }

        let object = self.alloc_from_byte_allocator(layout)?;
        self.record_usage_alloc(UsageKind::RustHeap, layout.size());
        Ok(object)
    }

    /// Allocate through the globally locked byte allocator.
    fn alloc_from_byte_allocator(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let mut balloc = self.balloc.lock();
        loop {
            let heap_ready = self.heap_ready.load(Ordering::Acquire);
            if heap_ready && let Ok(ptr) = balloc.allocate(layout) {
                return Ok(ptr);
            }

            self.grow_byte_allocator(&mut balloc, layout)?;
        }
    }

    /// Allocate a canonical slab object through the current CPU cache.
    #[cfg(feature = "slab")]
    fn alloc_from_percpu_slab(
        &self,
        size_class: alloc_engine::SizeClass,
    ) -> AllocResult<NonNull<u8>> {
        let _irq_guard = IrqSave::new();
        // SAFETY: `_irq_guard` excludes same-CPU interrupt re-entry for this
        // entire cache access. The per-CPU helper disables preemption while it
        // obtains the non-escaping mutable reference.
        if let Some(object) = unsafe { slab_cache::try_alloc(size_class) } {
            return Ok(object);
        }

        let class_size = size_class.size();
        let layout = Layout::from_size_align(class_size, class_size)
            .expect("slab size classes must be powers of two");
        let mut balloc = self.balloc.lock();
        loop {
            if self.heap_ready.load(Ordering::Acquire) {
                // SAFETY: local IRQs remain disabled, `balloc` is the central
                // slab allocator, and its lock gives exclusive mutable access.
                if let Ok(object) = unsafe { slab_cache::refill_cache(size_class, &mut *balloc) } {
                    return Ok(object);
                }
            }

            self.grow_byte_allocator(&mut balloc, layout)?;
        }
    }

    /// Add one region to the byte allocator, reducing the request under
    /// pressure until the minimum useful region size is reached.
    fn grow_byte_allocator(
        &self,
        balloc: &mut DefaultByteAllocator,
        layout: Layout,
    ) -> AllocResult {
        let heap_ready = self.heap_ready.load(Ordering::Acquire);
        let old_size = if heap_ready { balloc.total_bytes() } else { 0 };
        let exp_size = old_size
            .max(layout.size())
            .next_power_of_two()
            .max(PAGE_SIZE_4K);

        let mut req_size = exp_size;
        let min_size = if heap_ready {
            PAGE_SIZE_4K.max(layout.size())
        } else {
            MIN_HEAP_SIZE.max(layout.size())
        };
        loop {
            if !heap_ready {
                req_size = (req_size + MIN_HEAP_SIZE - 1) & !(MIN_HEAP_SIZE - 1);
            }
            let heap_addr = match self.alloc_pages(
                req_size / PAGE_SIZE_4K,
                PAGE_SIZE_4K,
                UsageKind::RustHeap,
            ) {
                Ok(addr) => addr,
                Err(err) => {
                    req_size /= 2;
                    if req_size < min_size {
                        return Err(err);
                    }
                    continue;
                }
            };
            debug!(
                "expand heap memory: [{:#x}, {:#x})",
                heap_addr,
                heap_addr + req_size
            );
            if heap_ready {
                balloc.add_region(heap_addr, req_size)?;
            } else {
                balloc.init_region(heap_addr, req_size);
                self.heap_ready.store(true, Ordering::Release);
            }
            return Ok(());
        }
    }

    /// Allocate directly from the page allocator for large requests.
    fn large_alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let pages = layout.size().div_ceil(PAGE_SIZE_4K);
        let align = layout.align().max(PAGE_SIZE_4K);
        let addr = self
            .alloc_pages(pages, align, UsageKind::RustHeap)
            .inspect_err(|e| {
                error!(
                    "large_alloc: failed to allocate {} pages ({} bytes): {:?}",
                    pages,
                    layout.size(),
                    e
                );
            })?;
        // alloc_pages skips usages for RustHeap; track it here.
        self.record_usage_alloc(UsageKind::RustHeap, pages_to_bytes(pages));
        // SAFETY: `addr` is non-null and properly aligned.
        Ok(unsafe { NonNull::new_unchecked(addr as *mut u8) })
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// The region should be allocated by [`alloc`], and `align_pow2` should be
    /// the same as the one used in [`alloc`]. Otherwise, the behavior is
    /// undefined.
    ///
    /// [`alloc`]: GlobalAllocator::alloc
    pub fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // Large allocations bypassed the byte allocator.
        if layout.size() > LARGE_ALLOC_THRESHOLD_BYTES {
            let pages = layout.size().div_ceil(PAGE_SIZE_4K);
            // dealloc_pages handles usages tracking internally.
            self.dealloc_pages(ptr.as_ptr() as usize, pages, UsageKind::RustHeap);
        } else {
            self.record_usage_dealloc(UsageKind::RustHeap, layout.size());

            #[cfg(feature = "slab")]
            if let Some(size_class) = alloc_engine::SizeClass::from_layout(layout) {
                let _irq_guard = IrqSave::new();
                // SAFETY: every eligible allocation is obtained from the
                // central allocator with this class's canonical layout. Local
                // IRQs stay disabled until the cache operation completes.
                if unsafe { slab_cache::try_free(size_class, ptr) } {
                    return;
                }

                let mut balloc = self.balloc.lock();
                // SAFETY: the pointer has the canonical class provenance
                // described above, local IRQs remain disabled, and `balloc`
                // is the exclusively locked central allocator.
                unsafe { slab_cache::drain_cache(size_class, &mut *balloc, ptr) };
                return;
            }

            self.balloc.lock().deallocate(ptr, layout);
        }
    }

    /// Allocates contiguous pages.
    ///
    /// It allocates `num_pages` pages from the page allocator.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    ///
    /// 1–4 page allocations with standard alignment are served from the
    /// per-CPU page cache to reduce global lock contention.
    pub fn alloc_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        debug_assert!(
            self.page_ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );

        // Fast path: 1–4 pages with standard alignment -> PCP cache.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) && align_pow2 <= PAGE_SIZE_4K {
            let _guard = IrqSave::new();
            if let Some(addr) = pcp::try_alloc(num_pages) {
                drop(_guard);
                if !matches!(kind, UsageKind::RustHeap) {
                    self.record_usage_alloc(kind, pages_to_bytes(num_pages));
                }
                return Ok(addr);
            }
            drop(_guard);

            // Cache miss: batch-fill from the global allocator.
            let mut palloc = self.palloc.lock();
            let result = pcp::fill_cache(num_pages, &mut palloc);
            drop(palloc);
            if result.is_ok() && !matches!(kind, UsageKind::RustHeap) {
                self.record_usage_alloc(kind, pages_to_bytes(num_pages));
            }
            return result;
        }

        // Slow path: multi-page or special alignment.
        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;
        if !matches!(kind, UsageKind::RustHeap) {
            self.record_usage_alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Allocates contiguous DMA pages.
    ///
    /// DMA allocations must come from physical memory below 4 GiB.
    /// The page allocator only knows virtual addresses, so the
    /// DMA32 filtering happens here using `v2p` translation.
    pub fn alloc_dma_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        debug_assert!(
            self.page_ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );
        const DMA32_LIMIT: usize = 0x1_0000_0000;

        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;

        let phys = kernel_virt_to_phys(addr);
        let size = pages_to_bytes(num_pages);
        if phys + size > DMA32_LIMIT {
            self.palloc.lock().deallocate_pages(addr, num_pages);
            return Err(AllocError::NoMemory);
        }

        if !matches!(kind, UsageKind::RustHeap) {
            self.record_usage_alloc(kind, size);
        }
        Ok(addr)
    }

    /// Allocates contiguous pages starting from the given address.
    ///
    /// It allocates `num_pages` pages from the page allocator starting from the
    /// given address.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    pub fn alloc_pages_at(
        &self,
        va: usize,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        let addr = self
            .palloc
            .lock()
            .allocate_pages_at(va, num_pages, align_pow2)?;
        if kind != UsageKind::RustHeap {
            self.record_usage_alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Gives back the allocated pages starts from `va` to the page allocator.
    ///
    /// The pages should be allocated by [`alloc_pages`], and `align_pow2`
    /// should be the same as the one used in [`alloc_pages`]. Otherwise, the
    /// behavior is undefined.
    ///
    /// 1–4 page deallocations are cached per-CPU to reduce global lock
    /// contention.
    ///
    /// [`alloc_pages`]: GlobalAllocator::alloc_pages
    pub fn dealloc_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.record_usage_dealloc(kind, pages_to_bytes(num_pages));

        // Fast path: 1–4 pages -> try to cache them per-CPU.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) {
            let _guard = IrqSave::new();
            if pcp::try_free(num_pages, va) {
                return;
            }
            drop(_guard);

            // Cache full: bulk-drain to the global allocator.
            let mut palloc = self.palloc.lock();
            pcp::drain_cache(num_pages, &mut palloc, va);
            return;
        }

        // Slow path: multi-page.
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Gives back the allocated DMA pages starts from `va` to the DMA page allocator.
    pub fn dealloc_dma_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.record_usage_dealloc(kind, pages_to_bytes(num_pages));
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Add diagnostic usage without placing allocation paths behind a usage lock.
    fn record_usage_alloc(&self, kind: UsageKind, size_bytes: usize) {
        with_current_usage_counters(|counters| counters.alloc(kind, size_bytes));
    }

    /// Record freed bytes for aggregate diagnostic accounting.
    fn record_usage_dealloc(&self, kind: UsageKind, size_bytes: usize) {
        with_current_usage_counters(|counters| counters.dealloc(kind, size_bytes));
    }

    fn init_or_extend_page_allocator(&self, va: usize, size: usize) -> AllocResult {
        if self.page_ready.load(Ordering::Acquire) {
            self.palloc.lock().add_region(va, size)?;
        } else {
            self.palloc.lock().init_region(va, size);
            self.page_ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn bootstrap_heap_if_needed(&self) -> AllocResult {
        if self.heap_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let heap_addr = self.alloc_pages(
            MIN_HEAP_SIZE / PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            UsageKind::RustHeap,
        )?;
        self.balloc.lock().init_region(heap_addr, MIN_HEAP_SIZE);
        self.heap_ready.store(true, Ordering::Release);
        Ok(())
    }

    // Note: The following delegation pattern is a standard Rust idiom for
    // thread-safe interior mutability, not a literal duplication.
    /// Returns the number of allocated bytes in the byte allocator.
    pub fn used_bytes(&self) -> usize {
        self.balloc.lock().used_bytes()
    }

    /// Returns the number of available bytes in the byte allocator.
    pub fn available_bytes(&self) -> usize {
        self.balloc.lock().available_bytes()
    }

    /// Returns the number of allocated pages in the page allocator.
    pub fn used_pages(&self) -> usize {
        self.palloc.lock().used_pages()
    }

    /// Returns the number of available pages in the page allocator.
    pub fn available_pages(&self) -> usize {
        self.palloc.lock().available_pages()
    }

    /// Returns caller-live usage aggregated from all per-CPU counters.
    ///
    /// The snapshot is not linearized against concurrent allocation and free;
    /// it may temporarily overstate or understate usage, but it is never used
    /// for allocator ownership or reclaim decisions.
    ///
    /// # Panics
    ///
    /// Panics if cumulative accounting overflows or total recorded frees
    /// exceed total recorded allocations for any [`UsageKind`].
    pub fn usages(&self) -> Usages {
        usage_snapshot()
    }
}

// SAFETY: `GlobalAllocator` routes all allocations through its internal locked
// allocators and upholds the `GlobalAlloc` contract for returned pointers.
unsafe impl GlobalAlloc for GlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let allocate_memory = || {
            if let Ok(ptr) = GlobalAllocator::alloc(self, layout) {
                ptr.as_ptr()
            } else {
                let used = self.used_bytes();
                let avail = self.available_bytes();
                error!(
                    "kalloc OOM: layout {:?} (size={}, align={}), heap used={:#x} avail={:#x}",
                    layout,
                    layout.size(),
                    layout.align(),
                    used,
                    avail
                );
                alloc::alloc::handle_alloc_error(layout)
            }
        };

        #[cfg(feature = "tracking")]
        {
            track_allocation(allocate_memory, layout)
        }

        #[cfg(not(feature = "tracking"))]
        {
            allocate_memory()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = NonNull::new(ptr).expect("dealloc null ptr");

        let deallocate_memory = || {
            GlobalAllocator::dealloc(self, ptr, layout);
        };

        #[cfg(feature = "tracking")]
        {
            track_deallocation(ptr, deallocate_memory);
        }

        #[cfg(not(feature = "tracking"))]
        {
            deallocate_memory();
        }
    }
}

#[cfg(feature = "tracking")]
fn track_allocation<F>(allocate_memory: F, layout: Layout) -> *mut u8
where
    F: FnOnce() -> *mut u8,
{
    tracking::with_state(|state| match state {
        None => allocate_memory(),
        Some(state) => {
            let ptr = allocate_memory();
            let generation = state.generation;
            state.generation += 1;
            state.map.insert(
                ptr as usize,
                tracking::AllocationInfo {
                    layout,
                    backtrace: backtrace::Backtrace::capture(),
                    generation,
                },
            );
            ptr
        }
    })
}

#[cfg(feature = "tracking")]
fn track_deallocation<F>(ptr: NonNull<u8>, deallocate_memory: F)
where
    F: FnOnce(),
{
    tracking::with_state(|state| match state {
        None => deallocate_memory(),
        Some(state) => {
            let address = ptr.as_ptr() as usize;
            state.map.remove(&address);
            deallocate_memory();
        }
    });
}

#[cfg_attr(all(target_os = "none", not(test)), global_allocator)]
static GLOBAL_ALLOCATOR: GlobalAllocator = GlobalAllocator::new();

/// Returns the reference to the global allocator.
pub fn global_allocator() -> &'static GlobalAllocator {
    &GLOBAL_ALLOCATOR
}

/// Initializes the global allocator with the given memory region.
///
/// Note that the memory region bounds are just numbers, and the allocator
/// does not actually access the region. Users should ensure that the region
/// is valid and not being used by others, so that the allocated memory is also
/// valid.
///
/// The first call bootstraps the allocator, and later calls extend it.
pub fn global_init(va: usize, size: usize) {
    debug!(
        "initialize global allocator at: [{:#x}, {:#x})",
        va,
        va + size
    );
    GLOBAL_ALLOCATOR.init(va, size);
}

/// Add the given memory region to the global allocator.
///
/// Users should ensure that the region is valid and not being used by others,
/// so that the allocated memory is also valid.
///
/// It's similar to [`global_init`], but can be called multiple times.
pub fn global_add_memory(va: usize, size: usize) -> AllocResult {
    debug!(
        "add a memory region to global allocator: [{:#x}, {:#x})",
        va,
        va + size
    );
    GLOBAL_ALLOCATOR.add_memory(va, size)
}

/// Return whether the global backing page allocator is ready for requests.
pub fn is_page_allocator_ready() -> bool {
    GLOBAL_ALLOCATOR.is_page_allocator_ready()
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_kalloc {
    use strum::VariantArray;
    use unittest::def_test;

    use super::{UsageCounters, UsageKind, UsageTotals};

    fn snapshot(counters: &[&UsageCounters]) -> super::Usages {
        let mut totals = UsageTotals::new();
        for counter in counters {
            totals.collect_freed(counter);
        }
        for counter in counters {
            totals.collect_allocated(counter);
        }
        totals.finish()
    }

    #[def_test]
    fn test_usages_alloc_dealloc() {
        let usages = UsageCounters::new();
        usages.alloc(UsageKind::RustHeap, 64);
        assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 64);
        usages.dealloc(UsageKind::RustHeap, 32);
        assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 32);
    }

    #[def_test]
    fn test_usage_kind_variants_len() {
        assert!(UsageKind::VARIANTS.len() >= 5);
    }

    #[def_test]
    fn test_usages_independent_kinds() {
        let usages = UsageCounters::new();
        usages.alloc(UsageKind::VirtMem, 10);
        usages.alloc(UsageKind::PageTable, 20);
        let snapshot = snapshot(&[&usages]);
        assert_eq!(snapshot.get(UsageKind::VirtMem), 10);
        assert_eq!(snapshot.get(UsageKind::PageTable), 20);
    }

    #[def_test]
    fn test_usages_allow_cross_cpu_free() {
        let allocating_cpu = UsageCounters::new();
        let freeing_cpu = UsageCounters::new();
        allocating_cpu.alloc(UsageKind::RustHeap, 64);
        freeing_cpu.dealloc(UsageKind::RustHeap, 64);

        assert_eq!(
            snapshot(&[&allocating_cpu, &freeing_cpu]).get(UsageKind::RustHeap),
            0
        );
    }
}
