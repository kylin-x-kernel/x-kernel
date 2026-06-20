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
    sync::atomic::{AtomicBool, Ordering},
};

#[allow(unused_imports)]
use alloc_engine::{AllocResult, BaseAllocator, BuddyPageAllocator, ByteAllocator, PageAllocator};
use buddy_slab_allocator::{
    SlabPoolTrait, SlabTrait,
    eii::{slab_pool_impl, virt_to_phys_impl},
};
use kaddr_layout::v2p;
use kspin::SpinNoIrq;
use memaddr::PAGE_SIZE_4K;
use strum::{IntoStaticStr, VariantArray};
const MIN_HEAP_SIZE: usize = 0x8000; // 32 K

fn pages_to_bytes(num_pages: usize) -> usize {
    num_pages
        .checked_mul(PAGE_SIZE_4K)
        .expect("page count byte size overflow")
}

#[virt_to_phys_impl]
fn kernel_virt_to_phys(vaddr: usize) -> usize {
    v2p(vaddr)
}

struct KernelSlabPool;
impl SlabTrait for KernelSlabPool {
    fn cpu_id(&self) -> usize {
        0
    }

    fn page_size(&self) -> usize {
        PAGE_SIZE_4K
    }

    fn alloc(
        &self,
        _layout: Layout,
    ) -> buddy_slab_allocator::AllocResult<buddy_slab_allocator::SlabAllocResult> {
        Err(buddy_slab_allocator::AllocError::NoMemory)
    }

    fn add_slab(&self, _size_class: buddy_slab_allocator::SizeClass, _base: usize, _bytes: usize) {}

    fn dealloc_local(
        &self,
        _ptr: NonNull<u8>,
        _layout: Layout,
    ) -> buddy_slab_allocator::SlabDeallocResult {
        buddy_slab_allocator::SlabDeallocResult::Done
    }
}
static KERNEL_SLAB_POOL: KernelSlabPool = KernelSlabPool;
impl SlabPoolTrait for KernelSlabPool {
    fn current_slab(&self) -> &dyn SlabTrait {
        &KERNEL_SLAB_POOL
    }

    fn owner_slab(&self, _cpu_idx: usize) -> &dyn SlabTrait {
        &KERNEL_SLAB_POOL
    }
}
#[slab_pool_impl]
fn kernel_slab_pool() -> &'static dyn SlabPoolTrait {
    &KERNEL_SLAB_POOL
}

mod page;
pub use page::GlobalPage;

#[cfg(feature = "tracking")]
mod tracking;
#[cfg(feature = "tracking")]
pub use tracking::*;

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
    /// Create a zero-initialized usage table.
    const fn new() -> Self {
        Self([0; UsageKind::VARIANTS.len()])
    }

    fn alloc(&mut self, kind: UsageKind, size: usize) {
        self.0[kind as usize] += size;
    }

    fn dealloc(&mut self, kind: UsageKind, size: usize) {
        self.0[kind as usize] = self.0[kind as usize]
            .checked_sub(size)
            .expect("kalloc usage underflow");
    }

    /// Return usage in bytes for the given kind.
    pub fn get(&self, kind: UsageKind) -> usize {
        self.0[kind as usize]
    }
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
/// Currently, [`TlsfByteAllocator`] is used as the byte allocator, while
/// [`BuddyPageAllocator`] is used as the page allocator.
///
/// [`TlsfByteAllocator`]: alloc_engine::TlsfByteAllocator
pub struct GlobalAllocator {
    balloc: SpinNoIrq<DefaultByteAllocator>,
    /// Whether the byte allocator already owns a bootstrap heap region.
    heap_ready: AtomicBool,
    /// Whether the page allocator has at least one usable memory region.
    page_ready: AtomicBool,
    palloc: SpinNoIrq<BuddyPageAllocator<{ PAGE_SIZE_4K }>>,
    usages: SpinNoIrq<Usages>,
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
            usages: SpinNoIrq::new(Usages::new()),
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

    pub fn init_page_allocator(&self, va: usize, size: usize) {
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
    }

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
    pub fn alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let mut balloc = self.balloc.lock();
        loop {
            let heap_ready = self.heap_ready.load(Ordering::Acquire);
            if heap_ready && let Ok(ptr) = balloc.allocate(layout) {
                self.usages.lock().alloc(UsageKind::RustHeap, layout.size());
                return Ok(ptr);
            }

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
                break;
            }
        }
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// The region should be allocated by [`alloc`], and `align_pow2` should be
    /// the same as the one used in [`alloc`]. Otherwise, the behavior is
    /// undefined.
    ///
    /// [`alloc`]: GlobalAllocator::alloc
    pub fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        self.usages
            .lock()
            .dealloc(UsageKind::RustHeap, layout.size());
        self.balloc.lock().deallocate(ptr, layout)
    }

    /// Allocates contiguous pages.
    ///
    /// It allocates `num_pages` pages from the page allocator.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
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
        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;
        if !matches!(kind, UsageKind::RustHeap) {
            self.usages.lock().alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Allocates contiguous DMA pages.
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
        let addr = self
            .palloc
            .lock()
            .allocate_pages_lowmem(num_pages, align_pow2)?;
        if !matches!(kind, UsageKind::RustHeap) {
            self.usages.lock().alloc(kind, pages_to_bytes(num_pages));
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
            self.usages.lock().alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Gives back the allocated pages starts from `va` to the page allocator.
    ///
    /// The pages should be allocated by [`alloc_pages`], and `align_pow2`
    /// should be the same as the one used in [`alloc_pages`]. Otherwise, the
    /// behavior is undefined.
    ///
    /// [`alloc_pages`]: GlobalAllocator::alloc_pages
    pub fn dealloc_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.usages.lock().dealloc(kind, pages_to_bytes(num_pages));
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Gives back the allocated DMA pages starts from `va` to the DMA page allocator.
    pub fn dealloc_dma_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.usages.lock().dealloc(kind, pages_to_bytes(num_pages));
        self.palloc.lock().deallocate_pages(va, num_pages);
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

    /// Returns the usage statistics of the allocator.
    pub fn usages(&self) -> Usages {
        *self.usages.lock()
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

pub fn is_page_allocator_ready() -> bool {
    GLOBAL_ALLOCATOR.is_page_allocator_ready()
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_kalloc {
    use strum::VariantArray;
    use unittest::def_test;

    use super::{UsageKind, Usages};

    #[def_test]
    fn test_usages_alloc_dealloc() {
        let mut usages = Usages::new();
        usages.alloc(UsageKind::RustHeap, 64);
        assert_eq!(usages.get(UsageKind::RustHeap), 64);
        usages.dealloc(UsageKind::RustHeap, 32);
        assert_eq!(usages.get(UsageKind::RustHeap), 32);
    }

    #[def_test]
    fn test_usage_kind_variants_len() {
        assert!(UsageKind::VARIANTS.len() >= 5);
    }

    #[def_test]
    fn test_usages_independent_kinds() {
        let mut usages = Usages::new();
        usages.alloc(UsageKind::VirtMem, 10);
        usages.alloc(UsageKind::PageTable, 20);
        assert_eq!(usages.get(UsageKind::VirtMem), 10);
        assert_eq!(usages.get(UsageKind::PageTable), 20);
    }
}
