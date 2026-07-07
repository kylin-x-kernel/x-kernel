// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic 64-bit multi-level page table implementation.
//!
//! This module provides [`PageTable64`] (read-only query) and [`PageTableMut`]
//! (mutable operations with deferred TLB flushes). Both are parameterized over
//! architecture-specific metadata (`M: PagingMetaData`), page table entry type
//! (`PTE: PageTableEntry`), and frame allocation handler (`H: PagingHandler`).
//!
//! # TLB flush batching
//!
//! `PageTableMut` batches TLB flushes for performance. Each mutating operation
//! (`map`, `unmap`, `remap`, `protect`) records the affected virtual address.
//! When the batch is finalized (via [`PageTableMut::finish`] or `Drop`), the
//! addresses are flushed either individually (≤ 16 entries) or with a full TLB
//! shootdown (> 16 entries).
#[cfg(any(
    target_arch = "aarch64",
    all(feature = "smp", not(target_arch = "aarch64"))
))]
use core::ptr::NonNull;
use core::{marker::PhantomData, ops::Deref};

use arrayvec::ArrayVec;
#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
use kcpu_id_map::KCpuMask;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr};

use crate::defs::{
    PageSize, PageTableEntry, PagingFlags, PagingHandler, PagingMetaData, PtError, PtResult,
    PteReplaceError, PteSnapshot, TlbFlushReceipt,
};

const ENTRY_COUNT: usize = 512;

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
struct UserAsidProvider {
    ctx: NonNull<()>,
    get_asid: unsafe fn(NonNull<()>) -> u16,
}

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
#[derive(Clone, Copy)]
struct UserCpuMaskProvider {
    ctx: NonNull<()>,
    get_mask: unsafe fn(NonNull<()>) -> KCpuMask,
}

#[cfg(target_arch = "aarch64")]
// SAFETY:
// - the provider context is installed only for address-space-owned ASID state
//   that outlives the page table using it;
// - the callback is restricted to read-only ASID fetches, so sharing the
//   provider across CPUs does not permit unsynchronized mutation.
unsafe impl Send for UserAsidProvider {}

#[cfg(target_arch = "aarch64")]
// SAFETY: same argument as `Send`; the provider only exposes read-only access
// to externally synchronized ASID state.
unsafe impl Sync for UserAsidProvider {}

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
// SAFETY:
// - the provider context is installed only for address-space-owned residency
//   state that outlives the page table using it;
// - the callback is restricted to taking a snapshot copy of that residency
//   state, so sharing it across CPUs does not create unsynchronized mutation.
unsafe impl Send for UserCpuMaskProvider {}

#[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
// SAFETY: same argument as `Send`; the provider only exposes snapshot reads of
// externally synchronized residency state.
unsafe impl Sync for UserCpuMaskProvider {}

const fn p4_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 27)) & (ENTRY_COUNT - 1)
}

const fn p3_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 18)) & (ENTRY_COUNT - 1)
}

const fn p2_idx(vaddr: usize) -> usize {
    (vaddr >> (12 + 9)) & (ENTRY_COUNT - 1)
}

const fn p1_idx(vaddr: usize) -> usize {
    (vaddr >> 12) & (ENTRY_COUNT - 1)
}

/// A 64-bit page table with configurable metadata and handlers.
///
/// `PageTable64` owns the root page table frame and all recursively allocated
/// sub-table frames. On [`Drop`], the entire frame tree is deallocated.
///
/// This type provides read-only operations (query). For mutable operations
/// (map, unmap, remap, protect), obtain a [`PageTableMut`] via [`modify`].
///
/// # Type parameters
///
/// - `M` — paging metadata (levels, address bits, TLB flush).
/// - `PTE` — architecture-specific page table entry type.
/// - `H` — frame allocator and phys-to-virt translation.
///
/// # Example
///
/// ```ignore
/// let pt: X64PageTable<H> = PageTable64::try_new()?;
/// if let Ok((paddr, flags, size)) = pt.query(vaddr) {
///     println!("vaddr -> paddr {paddr:?}, size {size:?}, flags {flags:?}");
/// }
/// ```
///
/// [`modify`]: PageTable64::modify
pub struct PageTable64<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> {
    root_paddr: PhysAddr,
    #[cfg(feature = "copy-from")]
    borrowed_entries: bitmaps::Bitmap<ENTRY_COUNT>,
    /// `true` for kernel page tables (shared globally).
    /// When set, [`PageTableMut::finish`] broadcasts TLB invalidations
    /// to **all** online CPUs instead of only the current task's
    /// residency mask.
    is_kernel: bool,
    #[cfg(target_arch = "aarch64")]
    user_asid_provider: Option<UserAsidProvider>,
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    user_cpu_mask_provider: Option<UserCpuMaskProvider>,
    _phantom: PhantomData<(M, PTE, H)>,
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTable64<M, PTE, H> {
    /// Create a new user page table root.
    ///
    /// # Errors
    ///
    /// Returns [`PtError::NoMemory`] if frame allocation fails.
    pub fn try_new() -> PtResult<Self> {
        Self::try_new_inner(false)
    }

    /// Create a new kernel page table root.
    ///
    /// Kernel page tables are shared across all processes, so TLB
    /// invalidations target **all** online CPUs rather than only the
    /// current task's residency mask.
    pub fn try_new_kernel() -> PtResult<Self> {
        Self::try_new_inner(true)
    }

    fn try_new_inner(is_kernel: bool) -> PtResult<Self> {
        let root_paddr = Self::alloc_table()?;
        Ok(Self {
            root_paddr,
            #[cfg(feature = "copy-from")]
            borrowed_entries: bitmaps::Bitmap::new(),
            is_kernel,
            #[cfg(target_arch = "aarch64")]
            user_asid_provider: None,
            #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
            user_cpu_mask_provider: None,
            _phantom: PhantomData,
        })
    }

    /// Registers a dynamic ASID provider for AArch64 user-page-table TLB invalidation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `ctx` remains valid for the full lifetime of this page table;
    /// - `get_asid(ctx)` performs only read-only access to that live context;
    /// - the provider returns the ASID currently paired with this page table's
    ///   user address-space root.
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn set_user_asid_provider(
        &mut self,
        ctx: NonNull<()>,
        get_asid: unsafe fn(NonNull<()>) -> u16,
    ) {
        self.user_asid_provider = Some(UserAsidProvider { ctx, get_asid });
    }

    /// Registers a CPU-residency provider for non-AArch64 user-page-table TLB targeting.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `ctx` remains valid for the full lifetime of this page table;
    /// - `get_mask(ctx)` performs only read-only access to that live context;
    /// - the provider returns the residency mask for this page table's user
    ///   address space.
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    pub unsafe fn set_user_cpu_mask_provider(
        &mut self,
        ctx: NonNull<()>,
        get_mask: unsafe fn(NonNull<()>) -> KCpuMask,
    ) {
        self.user_cpu_mask_provider = Some(UserCpuMaskProvider { ctx, get_mask });
    }

    /// Returns the physical address of the root page table frame.
    ///
    /// This is the address that should be loaded into the page table base
    /// register (e.g., CR3 on x86_64, TTBR0 on AArch64).
    pub const fn root_paddr(&self) -> PhysAddr {
        self.root_paddr
    }

    /// Queries the physical translation and flags for a virtual address.
    ///
    /// Walks the page table from the root to the leaf entry for `vaddr`.
    /// If the walk encounters a huge page at an intermediate level, the
    /// physical address is computed by aligning down and adding the page offset.
    ///
    /// # Returns
    ///
    /// - `Ok((paddr, flags, page_size))` — the physical address, permission
    ///   flags, and page size of the mapping.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — no present entry found for `vaddr`.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page
    ///   that blocks further walking (should not occur with correct PTE flags).
    /// - [`PtError::InvalidAddress`] — `vaddr` is not a valid canonical address.
    pub fn query(&self, vaddr: M::VirtAddr) -> PtResult<(PhysAddr, PagingFlags, PageSize)> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        let off = size.align_offset(vaddr.into());
        Ok((entry.paddr().add(off), entry.flags(), size))
    }

    /// Queries the present leaf entry that maps `vaddr`.
    ///
    /// Unlike [`query`](Self::query), this returns the base physical address
    /// encoded in the leaf PTE and a compare token suitable for
    /// [`PageTableMut::replace_if_same`].
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — no present leaf entry maps `vaddr`.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry blocks the walk.
    /// - [`PtError::InvalidAddress`] — `vaddr` is not a valid canonical address.
    pub fn query_entry(&self, vaddr: M::VirtAddr) -> PtResult<PteSnapshot> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        Ok(PteSnapshot::from_entry(entry, size))
    }

    /// Creates a mutable mapping view that tracks TLB flushes.
    ///
    /// The returned [`PageTableMut`] borrows `&mut self`, ensuring exclusive
    /// access. All mutating operations on the page table must go through this
    /// type. TLB flushes are deferred until [`PageTableMut::finish`] or `Drop`.
    pub fn modify(&mut self) -> PageTableMut<'_, M, PTE, H> {
        PageTableMut::new(self)
    }

    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    fn current_user_cpu_mask(&self) -> KCpuMask {
        self.user_cpu_mask_provider
            .map_or_else(KCpuMask::new, |provider| {
                // SAFETY: the provider contract guarantees that `ctx` stays valid
                // for the page table lifetime and that the callback performs only
                // a read-only snapshot of the residency state.
                unsafe { (provider.get_mask)(provider.ctx) }
            })
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTable64<M, PTE, H> {
    fn alloc_table() -> PtResult<PhysAddr> {
        if let Some(paddr) = H::alloc_frame() {
            let ptr = H::p2v(paddr).as_mut_ptr();
            // SAFETY: `H::alloc_frame()` returns a 4K-aligned physical frame.
            // `H::p2v()` returns a valid virtual address that uniquely maps the
            // frame with read-write access. The frame size is `PAGE_SIZE_4K`.
            unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE_4K) };
            Ok(paddr)
        } else {
            Err(PtError::NoMemory)
        }
    }

    fn table_of<'a>(&self, paddr: PhysAddr) -> &'a [PTE] {
        let ptr = H::p2v(paddr).as_ptr() as _;
        // SAFETY: `paddr` points to a valid 4K-aligned page table frame allocated
        // by `alloc_table()`. `H::p2v()` provides a valid virtual mapping of the
        // frame. The frame contains exactly `ENTRY_COUNT` (512) PTEs, which fits
        // within `PAGE_SIZE_4K` (512 × 8 = 4096 bytes). PTE types are `Copy`,
        // so no drop glue is involved.
        unsafe { core::slice::from_raw_parts(ptr, ENTRY_COUNT) }
    }

    fn next_table<'a>(&self, entry: &PTE) -> PtResult<&'a [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PtError::NotMapped)
        } else if entry.is_huge() {
            Err(PtError::MappedToHugePage)
        } else {
            Ok(self.table_of(entry.paddr()))
        }
    }

    fn get_entry(&self, vaddr: M::VirtAddr) -> PtResult<(&PTE, PageSize)> {
        crate::walk_page_table!(self, vaddr, table_of, next_table, ref)
    }

    fn dealloc_tree(&self, table_paddr: PhysAddr, level: usize) {
        if level < M::LEVELS - 1 {
            for entry in self.table_of(table_paddr) {
                if self.next_table(entry).is_ok() {
                    self.dealloc_tree(entry.paddr(), level + 1);
                }
            }
        }
        H::dealloc_frame(table_paddr);
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop for PageTable64<M, PTE, H> {
    fn drop(&mut self) {
        let root = self.table_of(self.root_paddr);
        #[allow(unused_variables)]
        for (i, entry) in root.iter().enumerate() {
            #[cfg(feature = "copy-from")]
            if self.borrowed_entries.get(i) {
                continue;
            }
            if self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
        }
        H::dealloc_frame(self.root_paddr());
    }
}

const FLUSH_THRESHOLD: usize = 16;

enum ToFlush<M: PagingMetaData> {
    None,
    Addresses(ArrayVec<M::VirtAddr, FLUSH_THRESHOLD>),
    Full,
}

impl<M: PagingMetaData> ToFlush<M> {
    const fn has_pending(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Mutable page table access with deferred TLB flushes.
///
/// `PageTableMut` borrows `&mut PageTable64` and provides map/unmap/remap/protect
/// operations. Each mutating operation records the affected virtual address for
/// deferred TLB flushing. The flushes are executed when [`finish`](Self::finish)
/// is called or when `PageTableMut` is dropped.
///
/// # TLB flush batching
///
/// Up to 16 addresses are flushed individually; beyond
/// that, a full TLB shootdown is performed. This avoids the overhead of
/// per-operation TLB invalidation during batch mappings.
///
/// # Example
///
/// ```ignore
/// let mut pt: X64PageTable<H> = PageTable64::try_new()?;
/// {
///     let mut m = pt.modify();
///     m.map(vaddr, paddr, PageSize::Size4K, PagingFlags::READ | PagingFlags::WRITE)?;
///     m.map(vaddr2, paddr2, PageSize::Size4K, PagingFlags::READ)?;
/// } // Drop flushes TLB automatically
/// ```
pub struct PageTableMut<'a, M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> {
    inner: &'a mut PageTable64<M, PTE, H>,
    flush: ToFlush<M>,
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Deref
    for PageTableMut<'_, M, PTE, H>
{
    type Target = PageTable64<M, PTE, H>;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'a, M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> PageTableMut<'a, M, PTE, H> {
    fn new(inner: &'a mut PageTable64<M, PTE, H>) -> Self {
        Self {
            inner,
            flush: ToFlush::None,
        }
    }

    fn flush(&mut self, vaddr: M::VirtAddr) {
        match self.flush {
            ToFlush::None => {
                let mut addresses = ArrayVec::new();
                addresses.push(vaddr);
                self.flush = ToFlush::Addresses(addresses);
            }
            ToFlush::Addresses(ref mut addrs) => {
                if addrs.try_push(vaddr).is_err() {
                    self.flush = ToFlush::Full;
                }
            }
            ToFlush::Full => {}
        }
    }

    fn table_of_mut(&mut self, paddr: PhysAddr) -> &'a mut [PTE] {
        let ptr = H::p2v(paddr).as_mut_ptr() as _;
        // SAFETY: Same as `table_of`, but `PageTableMut` holds `&mut PageTable64`,
        // so no other references to the frame exist. The frame layout and PTE
        // constraints are identical to `table_of`.
        unsafe { core::slice::from_raw_parts_mut(ptr, ENTRY_COUNT) }
    }

    fn next_table_mut(&mut self, entry: &PTE) -> PtResult<&'a mut [PTE]> {
        if entry.paddr().as_usize() == 0 {
            Err(PtError::NotMapped)
        } else if entry.is_huge() {
            Err(PtError::MappedToHugePage)
        } else {
            Ok(self.table_of_mut(entry.paddr()))
        }
    }

    fn next_table_mut_or_create(&mut self, entry: &mut PTE) -> PtResult<&'a mut [PTE]> {
        if entry.is_unused() {
            let paddr = PageTable64::<M, PTE, H>::alloc_table()?;
            *entry = PageTableEntry::new_table(paddr);
            Ok(self.table_of_mut(paddr))
        } else {
            self.next_table_mut(entry)
        }
    }

    fn get_entry_mut(&mut self, vaddr: M::VirtAddr) -> PtResult<(&mut PTE, PageSize)> {
        crate::walk_page_table!(self, vaddr, table_of_mut, next_table_mut, mut)
    }

    fn get_entry_mut_or_create(
        &mut self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PtResult<&mut PTE> {
        crate::walk_page_table_create!(self, vaddr, page_size)
    }

    #[cfg(target_arch = "aarch64")]
    fn current_user_asid(&self) -> u16 {
        self.inner.user_asid_provider.map_or(0, |provider| {
            // SAFETY:
            // - the owning address-space object installs a provider whose
            //   context outlives this page table;
            // - the callback performs a read-only fetch of the latest ASID.
            unsafe { (provider.get_asid)(provider.ctx) }
        })
    }

    /// Maps a virtual address to a physical address with the given page size and flags.
    ///
    /// Allocates intermediate page table frames as needed (via `H::alloc_frame`).
    /// The target entry must be unused; mapping over an existing entry returns
    /// [`PtError::AlreadyMapped`].
    ///
    /// # Errors
    ///
    /// - [`PtError::AlreadyMapped`] — `vaddr` is already mapped.
    /// - [`PtError::NoMemory`] — frame allocation for an intermediate table failed.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    /// - [`PtError::InvalidAddress`] — `vaddr` or `paddr` is outside the valid address range.
    pub fn map(
        &mut self,
        vaddr: M::VirtAddr,
        paddr: PhysAddr,
        page_size: PageSize,
        flags: PagingFlags,
    ) -> PtResult {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        if !M::paddr_is_valid(paddr.as_usize()) {
            return Err(PtError::InvalidAddress);
        }
        let entry = self.get_entry_mut_or_create(vaddr, page_size)?;
        if !entry.is_unused() {
            return Err(PtError::AlreadyMapped);
        }
        *entry = PageTableEntry::new_page(paddr.align_down(page_size), flags, page_size.is_huge());
        self.flush(vaddr);
        Ok(())
    }

    /// Remaps an existing mapping to a new physical address with new flags.
    ///
    /// The virtual address must already be mapped. The page size is preserved
    /// from the existing mapping.
    ///
    /// # Returns
    ///
    /// The [`PageSize`] of the remapped entry.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — `vaddr` is not mapped.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page
    ///   that blocks walking to the leaf.
    /// - [`PtError::InvalidAddress`] — `vaddr` or `paddr` is outside the valid address range.
    pub fn remap(
        &mut self,
        vaddr: M::VirtAddr,
        paddr: PhysAddr,
        flags: PagingFlags,
    ) -> PtResult<PageSize> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        if !M::paddr_is_valid(paddr.as_usize()) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        entry.set_paddr(paddr);
        entry.set_flags(flags, size.is_huge());
        self.flush(vaddr);
        Ok(size)
    }

    /// Replaces a present mapping only if the leaf entry still equals
    /// `expected`.
    ///
    /// This is the page-table commit primitive for COW-style transactions:
    /// higher layers may allocate or prepare a replacement page first, then
    /// commit the PTE only if the mapping observed before preparation has not
    /// changed. A changed PTE is reported without overwriting the current
    /// mapping, so callers can drop prepared resources and retry.
    ///
    /// The replacement preserves the page size from `expected`.
    ///
    /// # Returns
    ///
    /// The previous snapshot when the replacement succeeds.
    ///
    /// # Errors
    ///
    /// - [`PteReplaceError::Changed`] — the current entry no longer matches.
    /// - [`PteReplaceError::PageTable`] — invalid address, invalid physical
    ///   address, or page-table walk failure.
    pub fn replace_if_same(
        &mut self,
        vaddr: M::VirtAddr,
        expected: PteSnapshot,
        paddr: PhysAddr,
        flags: PagingFlags,
    ) -> Result<PteSnapshot, PteReplaceError> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PteReplaceError::PageTable(PtError::InvalidAddress));
        }
        if !M::paddr_is_valid(paddr.as_usize()) {
            return Err(PteReplaceError::PageTable(PtError::InvalidAddress));
        }

        let (entry, size) = match self.get_entry_mut(vaddr) {
            Ok(value) => value,
            Err(PtError::NotMapped) => return Err(PteReplaceError::Changed { current: None }),
            Err(err) => return Err(PteReplaceError::PageTable(err)),
        };
        if !entry.is_present() {
            return Err(PteReplaceError::Changed { current: None });
        }

        let current = PteSnapshot::from_entry(entry, size);
        if size != expected.page_size() || !expected.matches_entry(entry) {
            return Err(PteReplaceError::Changed {
                current: Some(current),
            });
        }

        entry.set_paddr(paddr.align_down(expected.page_size()));
        entry.set_flags(flags, expected.page_size().is_huge());
        self.flush(vaddr);
        Ok(current)
    }

    /// Changes the permission flags of an existing mapping.
    ///
    /// The virtual address must already be mapped (present). The page size
    /// and physical address are preserved.
    ///
    /// # Returns
    ///
    /// The [`PageSize`] of the protected entry.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — `vaddr` is not mapped.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    /// - [`PtError::InvalidAddress`] — `vaddr` is not a valid canonical address.
    pub fn protect(&mut self, vaddr: M::VirtAddr, flags: PagingFlags) -> PtResult<PageSize> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        if flags.is_empty() {
            entry.clear();
        } else {
            entry.set_flags(flags, size.is_huge());
        }
        self.flush(vaddr);
        Ok(size)
    }

    /// Unmaps a virtual address, clearing the leaf page table entry.
    ///
    /// # Returns
    ///
    /// The previous `(physical_address, flags, page_size)` of the unmapped entry.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — `vaddr` is not mapped.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    /// - [`PtError::InvalidAddress`] — `vaddr` is not a valid canonical address.
    pub fn unmap(&mut self, vaddr: M::VirtAddr) -> PtResult<(PhysAddr, PagingFlags, PageSize)> {
        if !M::vaddr_is_valid(vaddr.into()) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry_mut(vaddr)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        let paddr = entry.paddr();
        let flags = entry.flags();
        entry.clear();
        self.flush(vaddr);
        Ok((paddr, flags, size))
    }

    /// Maps a contiguous region of virtual addresses to physical addresses.
    ///
    /// Iterates through the region in page-sized steps, automatically selecting
    /// the largest possible page size (1G → 2M → 4K) based on alignment and
    /// remaining size. When `allow_huge` is `false`, only 4K pages are used.
    ///
    /// The physical address for each page is determined by `phys_getter`, which
    /// receives the virtual address and returns the corresponding physical address.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotAligned`] — `vaddr` or `size` is not 4K-aligned.
    /// - [`PtError::AlreadyMapped`] — a page in the region is already mapped.
    /// - [`PtError::NoMemory`] — frame allocation for an intermediate table failed.
    /// - [`PtError::InvalidAddress`] — `vaddr` or a physical address from `phys_getter` is invalid.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    ///
    /// # Note
    ///
    /// On partial failure, previously mapped pages within the region are **not**
    /// rolled back. The caller is responsible for cleanup.
    pub fn map_region(
        &mut self,
        vaddr: M::VirtAddr,
        phys_getter: impl Fn(M::VirtAddr) -> PhysAddr,
        size: usize,
        flags: PagingFlags,
        allow_huge: bool,
    ) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        if !PageSize::Size4K.is_aligned(vaddr_val) || !PageSize::Size4K.is_aligned(rem_size) {
            return Err(PtError::NotAligned);
        }
        if !M::vaddr_is_valid(vaddr_val) {
            return Err(PtError::InvalidAddress);
        }
        if vaddr_val
            .checked_add(rem_size - PageSize::Size4K as usize)
            .is_none()
        {
            return Err(PtError::InvalidAddress);
        }
        if !M::vaddr_is_valid(vaddr_val + rem_size - PageSize::Size4K as usize) {
            return Err(PtError::InvalidAddress);
        }
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let p_addr = phys_getter(v_addr);
            let p_size = if allow_huge {
                if PageSize::Size1G.is_aligned(vaddr_val)
                    && p_addr.is_aligned(PageSize::Size1G)
                    && rem_size >= PageSize::Size1G as usize
                {
                    PageSize::Size1G
                } else if PageSize::Size2M.is_aligned(vaddr_val)
                    && p_addr.is_aligned(PageSize::Size2M)
                    && rem_size >= PageSize::Size2M as usize
                {
                    PageSize::Size2M
                } else {
                    PageSize::Size4K
                }
            } else {
                PageSize::Size4K
            };
            self.map(v_addr, p_addr, p_size, flags)?;

            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    /// Unmaps a contiguous region of virtual addresses.
    ///
    /// Iterates through the region, unmapping each page and advancing by the
    /// returned page size.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotMapped`] — a page in the region is not mapped.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    pub fn unmap_region(&mut self, vaddr: M::VirtAddr, size: usize) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let (_, _, p_size) = self.unmap(v_addr)?;
            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    /// Changes permission flags for a contiguous region of virtual addresses.
    ///
    /// Unmapped pages within the region are silently skipped (the iterator
    /// advances by `PageSize::Size4K`).
    ///
    /// # Errors
    ///
    /// Returns an error only if `protect()` returns an error other than
    /// [`PtError::NotMapped`].
    pub fn protect_region(
        &mut self,
        vaddr: M::VirtAddr,
        size: usize,
        flags: PagingFlags,
    ) -> PtResult {
        let mut vaddr_val: usize = vaddr.into();
        let mut rem_size = size;
        while rem_size > 0 {
            let v_addr = vaddr_val.into();
            let p_size = match self.protect(v_addr, flags) {
                Ok(s) => s,
                Err(PtError::NotMapped) => PageSize::Size4K,
                Err(e) => return Err(e),
            };
            vaddr_val += p_size as usize;
            rem_size -= p_size as usize;
        }
        Ok(())
    }

    /// Copies top-level page table entries from `other` into this page table.
    ///
    /// This is used for `fork()` support: the child's page table inherits the
    /// parent's top-level entries (shared page table frames). Copied entries
    /// are marked in `borrowed_entries` so that `Drop` does not deallocate
    /// frames owned by the source page table.
    ///
    /// # Safety contract (caller responsibility)
    ///
    /// The source page table must outlive this page table. If the source is
    /// dropped first, the borrowed entries will point to freed frames.
    ///
    /// # Availability
    ///
    /// Only available with `feature = "copy-from"`.
    #[cfg(feature = "copy-from")]
    pub fn copy_from(&mut self, other: &PageTable64<M, PTE, H>, start: M::VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let src_table = self.table_of(other.root_paddr);
        let dst_table = self.table_of_mut(self.root_paddr);
        let index_fn = if M::LEVELS == 3 {
            p3_idx
        } else if M::LEVELS == 4 {
            p4_idx
        } else {
            unreachable!()
        };
        let start_idx = index_fn(start.into());
        let end_idx = index_fn(start.into() + size - 1) + 1;
        for i in start_idx..end_idx {
            let entry = &mut dst_table[i];
            if !self.inner.borrowed_entries.set(i, true) && self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
            *entry = src_table[i];
        }
    }

    /// Flushes all pending TLB entries and resets the flush state.
    ///
    /// - If ≤ 16 addresses were recorded, each is flushed individually.
    /// - If more than 16 addresses were recorded, a full TLB shootdown is performed.
    ///
    /// This method is also called automatically on `Drop`.
    pub fn finish(&mut self) -> TlbFlushReceipt {
        let had_pending = self.flush.has_pending();
        #[cfg(not(docsrs))]
        if self.inner.is_kernel {
            // Kernel page table: flush ALL online CPUs — the mapping is
            // shared across every process, so every CPU may hold stale
            // TLB entries.
            match &self.flush {
                ToFlush::None => {}
                ToFlush::Addresses(addrs) => {
                    for vaddr in addrs.iter() {
                        M::flush_tlb_all_cpus(Some(*vaddr));
                    }
                }
                ToFlush::Full => {
                    M::flush_tlb_all_cpus(None);
                }
            }
        } else {
            // User page table: invalidate stale entries for this address
            // space's ASID (AArch64) or residency mask (other arches).
            #[cfg(target_arch = "aarch64")]
            let asid = self.current_user_asid();
            #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
            let target_mask = self.current_user_cpu_mask();
            match &self.flush {
                ToFlush::None => {}
                ToFlush::Addresses(addrs) => {
                    for vaddr in addrs.iter() {
                        #[cfg(target_arch = "aarch64")]
                        M::flush_tlb_process_asid(Some(*vaddr), asid);
                        #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
                        M::flush_tlb_process_mask(Some(*vaddr), target_mask);
                        #[cfg(not(any(target_arch = "aarch64", feature = "smp")))]
                        M::flush_tlb_process(Some(*vaddr));
                    }
                }
                ToFlush::Full => {
                    #[cfg(target_arch = "aarch64")]
                    M::flush_tlb_process_asid(None, asid);
                    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
                    M::flush_tlb_process_mask(None, target_mask);
                    #[cfg(not(any(target_arch = "aarch64", feature = "smp")))]
                    M::flush_tlb_process(None);
                }
            }
        }
        self.flush = ToFlush::None;
        TlbFlushReceipt::new(had_pending)
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop
    for PageTableMut<'_, M, PTE, H>
{
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(unittest)]
mod tests {
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    use core::ptr::NonNull;
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    use core::sync::atomic::AtomicBool;
    use core::{
        cell::UnsafeCell,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    use kcpu_id_map::KCpuMask;
    use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
    use unittest::def_test;

    use crate::{
        PageSize, PageTable64, PageTableEntry, PagingFlags, PagingHandler, PagingMetaData,
        PteReplaceError,
    };

    #[derive(Clone, Copy)]
    #[repr(transparent)]
    struct TestEntry(usize);

    impl PageTableEntry for TestEntry {
        const EMPTY: Self = Self(0);

        fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
            let mut bits = paddr.as_usize() | (flags.bits() << Self::FLAGS_SHIFT) | Self::PRESENT;
            if is_huge {
                bits |= Self::HUGE;
            }
            Self(bits)
        }

        fn new_table(paddr: PhysAddr) -> Self {
            Self(paddr.as_usize() | Self::PRESENT | Self::TABLE)
        }

        fn paddr(&self) -> PhysAddr {
            PhysAddr::from(self.0 & Self::PADDR_MASK)
        }

        fn flags(&self) -> PagingFlags {
            PagingFlags::from_bits_truncate((self.0 & Self::FLAGS_MASK) >> Self::FLAGS_SHIFT)
        }

        fn set_paddr(&mut self, paddr: PhysAddr) {
            self.0 = (self.0 & !Self::PADDR_MASK) | paddr.as_usize();
        }

        fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
            self.0 = (self.0 & !(Self::FLAGS_MASK | Self::HUGE))
                | (flags.bits() << Self::FLAGS_SHIFT)
                | Self::PRESENT;
            if is_huge {
                self.0 |= Self::HUGE;
            }
        }

        fn bits(self) -> usize {
            self.0
        }

        fn is_present(&self) -> bool {
            self.0 & Self::PRESENT != 0
        }

        fn is_huge(&self) -> bool {
            self.0 & Self::HUGE != 0
        }
    }

    impl TestEntry {
        const FLAGS_MASK: usize = 0x7f << 3;
        const FLAGS_SHIFT: usize = 3;
        const HUGE: usize = 1 << 2;
        const PADDR_MASK: usize = !0xfff;
        const PRESENT: usize = 1 << 0;
        const TABLE: usize = 1 << 1;
    }

    impl core::fmt::Debug for TestEntry {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("TestEntry")
                .field("paddr", &self.paddr())
                .field("flags", &self.flags())
                .finish()
        }
    }

    struct TestMeta;

    impl PagingMetaData for TestMeta {
        type VirtAddr = VirtAddr;

        const LEVELS: usize = 4;
        const PA_MAX_BITS: usize = usize::BITS as usize - 1;
        const VA_MAX_BITS: usize = usize::BITS as usize - 1;

        fn vaddr_is_valid(_vaddr: usize) -> bool {
            true
        }

        fn paddr_is_valid(_paddr: usize) -> bool {
            true
        }

        fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}

        #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
        fn flush_tlb_process_mask(_vaddr: Option<Self::VirtAddr>, _target_mask: KCpuMask) {}
    }

    /// Per-instance provider context owned by a single test case.
    ///
    /// Moving the flush bookkeeping off module-level statics makes parallel
    /// test cases independent: each test instantiates its own `ProviderCtx`,
    /// so concurrent `PageTableMut::drop` → `flush_tlb_process_mask` paths in
    /// sibling tests can no longer pollute each other's counters. The
    /// previous global-static design raced under the parallel scheduler,
    /// inflating `flush_calls` with cross-test flushes.
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    struct ProviderCtx {
        /// Input residency mask that `test_user_cpu_mask_provider` snapshots.
        provider_mask_bits: [AtomicBool; kbuild_config::NR_CPUS],
        /// Number of times `flush_tlb_process_mask` was invoked.
        flush_calls: AtomicUsize,
        /// Last mask observed by `flush_tlb_process_mask`.
        observed_mask_bits: [AtomicBool; kbuild_config::NR_CPUS],
    }

    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    impl ProviderCtx {
        const fn new() -> Self {
            Self {
                provider_mask_bits: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
                flush_calls: AtomicUsize::new(0),
                observed_mask_bits: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
            }
        }
    }

    /// Provider callback that both returns the residency mask and records the
    /// observation into `ctx`. Counting here (rather than in
    /// `TestMeta::flush_tlb_process_mask`, which has no access to the test
    /// instance) makes each parallel test case self-contained: the counter
    /// lives on the test-owned `ProviderCtx`, not a shared module static.
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    unsafe fn test_user_cpu_mask_provider(ctx: NonNull<()>) -> KCpuMask {
        // SAFETY: the caller (PageTable64::set_user_cpu_mask_provider) upholds
        // the provider contract; `ctx` points to a live `ProviderCtx`.
        let ctx = unsafe { ctx.cast::<ProviderCtx>().as_ref() };
        let mut mask = KCpuMask::new();
        for (cpu, provider_bit) in ctx.provider_mask_bits.iter().enumerate() {
            mask.set(cpu, provider_bit.load(Ordering::Relaxed));
        }
        // Record this observation. `finish()` calls `current_user_cpu_mask()`
        // (this provider) and `flush_tlb_process_mask` in lockstep, so a
        // provider invocation corresponds one-to-one with a process-mask
        // TLB flush.
        ctx.flush_calls.fetch_add(1, Ordering::Relaxed);
        for (cpu, observed_bit) in ctx.observed_mask_bits.iter().enumerate() {
            observed_bit.store(mask.get(cpu), Ordering::Relaxed);
        }
        mask
    }

    #[repr(align(4096))]
    #[derive(Clone, Copy)]
    struct TestFrame {
        _bytes: [u8; PAGE_SIZE_4K],
    }

    struct FramePool(UnsafeCell<[TestFrame; 64]>);

    // SAFETY: The unit-test frame allocator publishes frames by monotonically
    // advancing `NEXT_FRAME`; each successful allocation receives a distinct
    // slot and `dealloc_frame` never returns slots to the pool. Concurrent test
    // allocations can therefore race only on `NEXT_FRAME`, not on the same
    // `UnsafeCell` element.
    unsafe impl Sync for FramePool {}

    static NEXT_FRAME: AtomicUsize = AtomicUsize::new(0);
    static FRAME_POOL: FramePool = FramePool(UnsafeCell::new(
        [TestFrame {
            _bytes: [0; PAGE_SIZE_4K],
        }; 64],
    ));

    struct TestHandler;

    impl PagingHandler for TestHandler {
        fn alloc_frame() -> Option<PhysAddr> {
            let index = NEXT_FRAME.fetch_add(1, Ordering::Relaxed);
            if index >= 64 {
                return None;
            }
            // SAFETY: Unit tests allocate each frame at most once by advancing
            // `NEXT_FRAME`. `FramePool` is 4K-aligned and each slot is exactly
            // one page, so the returned address satisfies `PagingHandler`.
            let ptr = unsafe { (*FRAME_POOL.0.get()).as_mut_ptr().add(index) };
            Some(PhysAddr::from(ptr as usize))
        }

        fn dealloc_frame(_paddr: PhysAddr) {}

        fn p2v(paddr: PhysAddr) -> VirtAddr {
            VirtAddr::from(paddr.as_usize())
        }
    }

    type TestPageTable = PageTable64<TestMeta, TestEntry, TestHandler>;

    fn paddr(value: usize) -> PhysAddr {
        PhysAddr::from(value)
    }

    fn vaddr(value: usize) -> VirtAddr {
        VirtAddr::from(value)
    }

    #[def_test]
    fn query_entry_returns_leaf_snapshot() {
        let mut table = TestPageTable::try_new().expect("test page table");
        let page = paddr(0x20_0000);
        table
            .modify()
            .map(
                vaddr(0x4000),
                page,
                PageSize::Size4K,
                PagingFlags::READ | PagingFlags::WRITE,
            )
            .expect("map test page");

        let snapshot = table.query_entry(vaddr(0x4123)).expect("query entry");

        assert_eq!(snapshot.paddr(), page);
        assert_eq!(snapshot.page_size(), PageSize::Size4K);
        assert_eq!(snapshot.flags(), PagingFlags::READ | PagingFlags::WRITE);
    }

    #[def_test]
    fn replace_if_same_commits_only_matching_snapshot() {
        let mut table = TestPageTable::try_new().expect("test page table");
        let old_page = paddr(0x30_0000);
        let new_page = paddr(0x31_0000);
        table
            .modify()
            .map(vaddr(0x8000), old_page, PageSize::Size4K, PagingFlags::READ)
            .expect("map test page");
        let expected = table.query_entry(vaddr(0x8000)).expect("query entry");

        let old_snapshot = table
            .modify()
            .replace_if_same(
                vaddr(0x8000),
                expected,
                new_page,
                PagingFlags::READ | PagingFlags::WRITE,
            )
            .expect("replace matching entry");

        assert_eq!(old_snapshot.paddr(), old_page);
        assert_eq!(
            table.query(vaddr(0x8000)).expect("query replaced mapping"),
            (
                new_page,
                PagingFlags::READ | PagingFlags::WRITE,
                PageSize::Size4K,
            )
        );
    }

    #[def_test]
    fn replace_if_same_reports_changed_without_overwrite() {
        let mut table = TestPageTable::try_new().expect("test page table");
        let original_page = paddr(0x40_0000);
        let competing_page = paddr(0x41_0000);
        let prepared_page = paddr(0x42_0000);
        table
            .modify()
            .map(
                vaddr(0xc000),
                original_page,
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .expect("map test page");
        let stale = table.query_entry(vaddr(0xc000)).expect("query entry");
        table
            .modify()
            .remap(vaddr(0xc000), competing_page, PagingFlags::READ)
            .expect("competing remap");

        let result = table.modify().replace_if_same(
            vaddr(0xc000),
            stale,
            prepared_page,
            PagingFlags::READ | PagingFlags::WRITE,
        );

        assert!(matches!(
            result,
            Err(PteReplaceError::Changed { current: Some(current) })
                if current.paddr() == competing_page
        ));
        assert_eq!(
            table.query(vaddr(0xc000)).expect("query unchanged mapping"),
            (competing_page, PagingFlags::READ, PageSize::Size4K)
        );
    }

    #[def_test]
    fn finish_reports_explicit_flush_boundary() {
        let mut table = TestPageTable::try_new().expect("test page table");
        let mut modify = table.modify();

        assert!(!modify.finish().had_pending());
        modify
            .map(
                vaddr(0x10_0000),
                paddr(0x50_0000),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .expect("map test page");
        assert!(modify.finish().had_pending());
        assert!(!modify.finish().had_pending());
    }

    /// Verifies that an installed CPU-residency provider is consulted exactly
    /// once per pending `finish()` flush, and that the mask it returns is the
    /// one delivered to `flush_tlb_process_mask`.
    ///
    /// All bookkeeping lives on the stack-local `ProviderCtx`, so this case is
    /// independent of any sibling test running under the parallel scheduler.
    #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
    #[def_test]
    fn user_page_table_flush_uses_installed_process_mask() {
        // The provider context outlives the page table, satisfying the
        // provider lifetime contract.
        let mut ctx = ProviderCtx::new();

        let mut expected_targets = [false; kbuild_config::NR_CPUS];
        let first_nonzero_cpu = 1;
        let primary_target_cpu = if kbuild_config::NR_CPUS > 1 {
            first_nonzero_cpu
        } else {
            0
        };
        ctx.provider_mask_bits[primary_target_cpu].store(true, Ordering::Relaxed);
        expected_targets[primary_target_cpu] = true;

        let secondary_target_cpu = kbuild_config::NR_CPUS.saturating_sub(1);
        if secondary_target_cpu != primary_target_cpu {
            ctx.provider_mask_bits[secondary_target_cpu].store(true, Ordering::Relaxed);
            expected_targets[secondary_target_cpu] = true;
        }

        let mut table = TestPageTable::try_new().expect("test page table");
        let ctx_ptr = NonNull::from(&mut ctx).cast();
        // SAFETY: `ctx` lives on this stack frame for the entire lifetime of
        // `table`, and `test_user_cpu_mask_provider` performs only the read +
        // bookkeeping documented on `ProviderCtx`.
        unsafe { table.set_user_cpu_mask_provider(ctx_ptr, test_user_cpu_mask_provider) };

        table
            .modify()
            .map(
                vaddr(0x20_0000),
                paddr(0x40_0000),
                PageSize::Size4K,
                PagingFlags::READ,
            )
            .expect("map test page");

        // `finish()` (via `Drop`) consults the provider once for the single
        // pending address, so exactly one observation must be recorded.
        assert_eq!(ctx.flush_calls.load(Ordering::Relaxed), 1);
        for (cpu, observed_bit) in ctx.observed_mask_bits.iter().enumerate() {
            assert_eq!(observed_bit.load(Ordering::Relaxed), expected_targets[cpu]);
        }
    }
}
