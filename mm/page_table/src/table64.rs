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
    PteReplaceError, PteSnapshot, TlbFlushReceipt, TlbFlushResult,
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

mod retirement;
use retirement::RetiredReferences;

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
/// sub-table frames. Managed leaves additionally own data lifetime holds;
/// raw leaves do not. On [`Drop`], the frame tree and remaining data holds are
/// released. Callers must stop hardware use, exclude future reactivation and
/// complete required translation retirement before dropping the table.
/// AArch64 user tables with a registered ASID provider invalidate that ASID's
/// translations, including intermediate walk-cache entries, before freeing frames.
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

    /// Returns whether this table owns the global kernel mappings.
    ///
    /// Kernel tables invalidate through the all-CPU scope; user tables use the
    /// owning address space's residency mask.
    pub const fn is_kernel(&self) -> bool {
        self.is_kernel
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
    /// The provider is consulted when flushing page-table changes and when
    /// dropping the user page table, before any owned table frames are freed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `ctx` remains valid for the full lifetime of this page table, including
    ///   its destructor;
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
    /// type. Automatic retirement advances at finish, Drop and bounded drains;
    /// try_finish provides an explicit synchronous completion boundary.
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
            // A hardware walker does not acquire the address-space lock. Complete
            // the clear before a parent descriptor or TTBR can expose this frame;
            // a barrier in `finish()` would run after that publication window.
            // This explicit DSB is scoped to the verified AArch64 KVM regression.
            // Other backends retain their existing protocols; their publication
            // ordering is outside this fix's architecture audit (docs/design.md).
            #[cfg(target_arch = "aarch64")]
            karch::dsb_ishst();
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
        } else if !entry.is_table() {
            Err(PtError::NotMapped)
        } else {
            Ok(self.table_of(entry.paddr()))
        }
    }

    fn get_entry(&self, vaddr: M::VirtAddr) -> PtResult<(&PTE, PageSize)> {
        crate::walk_page_table!(self, vaddr, table_of, next_table, ref)
    }

    fn dealloc_entry(&self, entry: &PTE, level: usize) {
        if entry.is_unused() {
            return;
        }
        if level == M::LEVELS - 1 || entry.is_huge() {
            if entry.is_managed() {
                let bytes = 1usize << (12 + 9 * (M::LEVELS - 1 - level));
                // SAFETY: This inactive owned subtree is being destroyed once,
                // after required hardware retirement. Borrowed roots are skipped
                // by Drop and copy_from rejects live managed destination leaves.
                drop(unsafe { H::mapping_reference_from_raw(entry.paddr(), bytes) });
            }
        } else if entry.is_table() {
            self.dealloc_tree(entry.paddr(), level + 1);
        }
    }

    fn dealloc_tree(&self, table_paddr: PhysAddr, level: usize) {
        for entry in self.table_of(table_paddr) {
            self.dealloc_entry(entry, level);
        }
        H::dealloc_frame(table_paddr);
    }

    #[cfg(feature = "copy-from")]
    fn entry_has_managed_leaf(&self, entry: &PTE, level: usize) -> bool {
        if entry.is_unused() {
            return false;
        }
        if level == M::LEVELS - 1 || entry.is_huge() {
            return entry.is_managed();
        }
        entry.is_table()
            && self
                .table_of(entry.paddr())
                .iter()
                .any(|child| self.entry_has_managed_leaf(child, level + 1))
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop for PageTable64<M, PTE, H> {
    fn drop(&mut self) {
        #[cfg(target_arch = "aarch64")]
        if !self.is_kernel
            && let Some(provider) = self.user_asid_provider
        {
            // SAFETY: The provider contract keeps its context alive through
            // page-table destruction and permits this read-only ASID fetch.
            let asid = unsafe { (provider.get_asid)(provider.ctx) };
            // Leaf invalidations cannot retire cached intermediate table
            // descriptors before their physical frames are reused.
            karch::dsb_ishst();
            if M::flush_tlb_process_asid(None, asid).is_err() {
                // A failed final invalidation cannot authorize reuse of table
                // frames or mapped data. Preserve all raw frame transfers too.
                // The unrecovered leaf transfers remain pinned with the tree.
                return;
            }
        }
        let root = self.table_of(self.root_paddr);
        #[expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: The root-entry index is used only with the \
                      copy-from feature"
        )]
        #[allow(
            unused_variables,
            reason = "CG-024-lint-exceptions: unused_variables: The root-entry index is used only \
                      with the copy-from feature"
        )]
        for (i, entry) in root.iter().enumerate() {
            #[cfg(feature = "copy-from")]
            if self.borrowed_entries.get(i) {
                continue;
            }
            self.dealloc_entry(entry, 0);
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

/// Kernel-mapping invalidation detached from a [`PageTableMut`] guard.
///
/// The page-table writes are already published when this value is produced, so
/// the caller may release the lock that protects the table and then call
/// [`complete`](Self::complete). That ordering matters when the invalidation
/// waits for remote CPUs: a CPU that needs the same lock must not be spinning
/// with interrupts disabled, or it can never acknowledge the shootdown.
///
/// Dropping the value without completing still performs the invalidation, but
/// it then runs at the drop site, which may still hold the lock, and its result
/// is not reported. Callers that must observe completion use
/// [`complete`](Self::complete).
pub struct DeferredKernelTlbFlush {
    scope: Option<DeferredKernelTlbFlushScope>,
    flush_all_cpus: fn(Option<usize>) -> TlbFlushResult,
}

enum DeferredKernelTlbFlushScope {
    Full,
    Addresses(alloc::vec::Vec<usize>),
}

impl DeferredKernelTlbFlush {
    /// Performs the detached invalidation and reports whether every relevant
    /// CPU confirmed it.
    ///
    /// A failed scoped or partial shootdown is retried once as a full-TLB
    /// invalidation, mirroring the guard's `ToFlush::Full` retry. The caller
    /// must not treat the mapping change as complete when this returns an
    /// error.
    pub fn complete(mut self) -> TlbFlushResult {
        self.invalidate()
    }

    fn invalidate(&mut self) -> TlbFlushResult {
        let Some(scope) = self.scope.take() else {
            return Ok(());
        };
        let result = match scope {
            DeferredKernelTlbFlushScope::Full => (self.flush_all_cpus)(None),
            DeferredKernelTlbFlushScope::Addresses(addresses) => {
                let mut result = Ok(());
                for address in addresses {
                    let current = (self.flush_all_cpus)(Some(address));
                    if current.is_err() {
                        result = current;
                    }
                }
                result
            }
        };
        if result.is_ok() {
            return Ok(());
        }
        // A partial or scoped failure cannot authorize reuse; retry the whole
        // TLB once, as the guard would retain a full-scope retry.
        (self.flush_all_cpus)(None)
    }
}

impl Drop for DeferredKernelTlbFlush {
    fn drop(&mut self) {
        // Best effort: a drop site cannot report failure. Callers that need
        // the result call `complete`.
        let _ = self.invalidate();
    }
}

/// Mutable page table access with deferred TLB flushes.
///
/// `PageTableMut` borrows `&mut PageTable64` and provides map/unmap/remap/protect
/// operations. Each mutating operation records the affected virtual address for
/// deferred TLB flushing. Managed data holds are retained after leaf removal
/// until the required synchronous invalidation completes. Finalization occurs
/// at [`finish`](Self::finish), Drop, or a bounded retirement-storage drain.
///
/// # TLB flush batching
///
/// Up to 16 addresses are flushed individually; beyond
/// that, a full TLB shootdown is performed. This avoids the overhead of
/// per-operation TLB invalidation during batch mappings. Duplicate addresses
/// share an invalidation; huge-leaf changes conservatively request a full flush.
/// Retirement storage is independent: 16 inline holds, up to 256 with fallible
/// heap spill. Reaching that bound or failing to spill completes a batch early.
///
/// # Example
///
/// ```ignore
/// let mut pt: X64PageTable<H> = PageTable64::try_new()?;
/// {
///     let mut m = pt.modify();
///     m.map(vaddr, reference, PagingFlags::READ | PagingFlags::WRITE)?;
///     m.map(vaddr2, reference2, PagingFlags::READ)?;
/// } // Drop flushes TLB automatically
/// ```
pub struct PageTableMut<'a, M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> {
    inner: &'a mut PageTable64<M, PTE, H>,
    flush: ToFlush<M>,
    /// Whether an entry was installed into a previously unused slot on an
    /// architecture that needs no invalidation for that transition.
    fresh_map: bool,
    retired_mapping_refs: RetiredReferences<H::MappingReference>,
    #[cfg(unittest)]
    sparse_entries_visited: usize,
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
            fresh_map: false,
            retired_mapping_refs: RetiredReferences::new(),
            #[cfg(unittest)]
            sparse_entries_visited: 0,
        }
    }

    fn queue_retired_reference(&mut self, reference: H::MappingReference) {
        if !self.flush.has_pending() {
            // A multi-hold replacement may have drained while retiring its
            // first hold. This still-local hold shared that completed change.
            drop(reference);
            return;
        }
        match self.retired_mapping_refs.try_push(reference) {
            Ok(()) => {
                if self.retired_mapping_refs.is_full() {
                    let _ = self.flush_pending();
                }
            }
            Err(reference) => {
                // The PTE change is already in the summary. Keep this hold
                // live through completion, then release it with this batch;
                // carrying it into the next batch would skew the boundaries.
                if self.flush_pending().is_ok() {
                    drop(reference);
                } else {
                    // No spill capacity and no confirmed retirement: retaining
                    // this hold permanently is safer than recycling its page.
                    core::mem::forget(reference);
                }
            }
        }
    }

    fn retire_mapping_reference(&mut self, old_entry: PTE, size: PageSize) {
        if old_entry.is_managed() {
            // SAFETY: The removed owned leaf uniquely held this exact transfer.
            // Its PTE change is already queued for invalidation; the recovered
            // hold is moved into retirement before it can be released.
            let reference =
                unsafe { H::mapping_reference_from_raw(old_entry.paddr(), size as usize) };
            self.queue_retired_reference(reference);
        }
    }

    fn flush_leaf(&mut self, vaddr: M::VirtAddr, size: PageSize) {
        if size.is_huge() {
            // A huge leaf can have cached translations throughout its range;
            // one base-page invalidation is not a portable retirement boundary.
            self.flush = ToFlush::Full;
        } else {
            self.flush(vaddr);
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
                if !addrs.contains(&vaddr) && addrs.try_push(vaddr).is_err() {
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
        } else if !entry.is_table() {
            Err(PtError::NotMapped)
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

    fn ensure_mutable_subtree(&self, vaddr: M::VirtAddr) -> PtResult {
        #[cfg(feature = "copy-from")]
        {
            let address = vaddr.into();
            let index = if M::LEVELS == 3 {
                p3_idx(address)
            } else {
                p4_idx(address)
            };
            if self.inner.borrowed_entries.get(index) {
                return Err(PtError::InvalidAddress);
            }
        }
        #[cfg(not(feature = "copy-from"))]
        let _ = vaddr;
        Ok(())
    }

    fn get_entry_mut(&mut self, vaddr: M::VirtAddr) -> PtResult<(&mut PTE, PageSize)> {
        self.ensure_mutable_subtree(vaddr)?;
        crate::walk_page_table!(self, vaddr, table_of_mut, next_table_mut, mut)
    }

    fn get_entry_mut_or_create(
        &mut self,
        vaddr: M::VirtAddr,
        page_size: PageSize,
    ) -> PtResult<&mut PTE> {
        self.ensure_mutable_subtree(vaddr)?;
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
    /// This installs no mapping hold. The backing owner must keep the entire
    /// range live through removal and confirmed invalidation; `finish()` alone
    /// is not a release proof. Use [`map`](Self::map) or
    /// [`map_sized`](Self::map_sized) to transfer that responsibility here.
    ///
    /// # Errors
    ///
    /// - [`PtError::AlreadyMapped`] — `vaddr` is already mapped.
    /// - [`PtError::NoMemory`] — frame allocation for an intermediate table failed.
    /// - [`PtError::MappedToHugePage`] — an intermediate entry is a huge page.
    /// - [`PtError::InvalidAddress`] — `vaddr` or `paddr` is outside the valid address range.
    pub fn map_raw(
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
        // The entry was verified unused above, so architectures that cannot
        // cache a translation for an invalid entry need no invalidation here;
        // they only owe the other walkers an ordered entry write, which
        // `finish` emits once for the whole batch.
        if M::INVALID_TO_VALID_NEEDS_TLBI {
            self.flush_leaf(vaddr, page_size);
        } else {
            self.fresh_map = true;
        }
        Ok(())
    }

    /// Maps one 4 KiB data page while retaining its supplied lifetime hold.
    ///
    /// Unmapping, sparse invalidation and protection that clears the PTE move
    /// the hold to the retirement batch. It is dropped only after `finish`
    /// completes the required invalidation. Dropping an inactive page table
    /// also releases its remaining holds under the table's destruction contract.
    /// A failed map releases the supplied hold without installing a leaf. Page contents require separate access
    /// synchronization.
    ///
    /// # Errors
    /// Returns the same errors as [`map_sized`](Self::map_sized).
    /// The selected hold must describe exactly one base page.
    pub fn map(
        &mut self,
        vaddr: M::VirtAddr,
        reference: impl Into<H::MappingReference>,
        flags: PagingFlags,
    ) -> PtResult {
        self.map_sized(vaddr, reference.into(), PageSize::Size4K, flags)
    }

    /// Maps one leaf with a hold covering exactly its contiguous physical size.
    ///
    /// The reference covers every backing base page of a 4 KiB or larger leaf.
    /// It is retired after invalidation, without splitting a huge leaf or using
    /// reference counts as a compound-page protocol. Empty flags install no
    /// leaf and drop the supplied hold after validation; an existing present
    /// mapping still returns AlreadyMapped. A caller that later restores
    /// access must retain its backing independently for fault rematerialization.
    ///
    /// # Errors
    /// Returns NotAligned for mismatched reference size or alignment, and the
    /// same errors as map_raw for invalid addresses, occupied leaves or table-frame
    /// allocation. Borrowed subtrees reject mutation with InvalidAddress.
    ///
    /// Table-frame allocation is the only allocation needed for admission;
    /// the leaf itself records the transferred hold without an owner index.
    pub fn map_sized(
        &mut self,
        vaddr: M::VirtAddr,
        reference: H::MappingReference,
        size: PageSize,
        flags: PagingFlags,
    ) -> PtResult {
        let paddr = Self::validate_reference(vaddr, &reference, size)?;
        self.ensure_mutable_subtree(vaddr)?;
        if flags.is_empty() {
            return match self.query(vaddr) {
                Ok(_) => Err(PtError::AlreadyMapped),
                Err(PtError::NotMapped) => Ok(()),
                Err(error) => Err(error),
            };
        }
        let entry = self.get_entry_mut_or_create(vaddr, size)?;
        if !entry.is_unused() {
            return Err(PtError::AlreadyMapped);
        }
        let mut replacement = PTE::new_page(paddr, flags, size.is_huge());
        replacement.set_managed(true);
        // All fallible preparation finished. The descriptor now owns this
        // exact transfer; publishing it does not acquire another reference.
        let _transfer = H::mapping_reference_into_raw(reference);
        *entry = replacement;
        if M::INVALID_TO_VALID_NEEDS_TLBI {
            self.flush_leaf(vaddr, size);
        } else {
            self.fresh_map = true;
        }
        Ok(())
    }

    /// Duplicates a present leaf without consuming its source ownership.
    ///
    /// A managed source acquires an independent hold through the provider before
    /// destination publication. A raw source remains externally owned: its caller
    /// must keep that backing alive for both mappings and their retirement.
    /// Source and destination must name aligned leaf bases, not interior bytes.
    ///
    /// # Errors
    /// Returns NotMapped for an absent source, NotAligned for leaf misalignment,
    /// AlreadyMapped for an occupied destination, InvalidAddress for invalid or
    /// borrowed ranges or unsupported managed cloning, and NoMemory for clone
    /// overflow or table allocation failure. The source is unchanged on failure.
    pub fn map_existing(
        &mut self,
        source: M::VirtAddr,
        destination: M::VirtAddr,
    ) -> PtResult<PageSize> {
        let source_address: usize = source.into();
        let destination_address: usize = destination.into();
        if !M::vaddr_is_valid(source_address) || !M::vaddr_is_valid(destination_address) {
            return Err(PtError::InvalidAddress);
        }
        let (entry, size) = self.get_entry_mut(source)?;
        if !entry.is_present() {
            return Err(PtError::NotMapped);
        }
        let entry = *entry;
        if !size.is_aligned(source_address) || !size.is_aligned(destination_address) {
            return Err(PtError::NotAligned);
        }
        let last = destination_address
            .checked_add(size as usize - 1)
            .ok_or(PtError::InvalidAddress)?;
        if !M::vaddr_is_valid(last) {
            return Err(PtError::InvalidAddress);
        }
        self.ensure_mutable_subtree(destination)?;
        match self.query(destination) {
            Ok(_) => return Err(PtError::AlreadyMapped),
            Err(PtError::NotMapped) => {}
            Err(error) => return Err(error),
        }
        if entry.is_managed() {
            // SAFETY: Exclusive PageTableMut access keeps this exact live leaf
            // and its transferred hold unchanged throughout provider cloning.
            let reference =
                unsafe { H::mapping_reference_try_clone_from_raw(entry.paddr(), size as usize) }?;
            self.map_sized(destination, reference, size, entry.flags())?;
        } else {
            self.map_raw(destination, entry.paddr(), size, entry.flags())?;
        }
        Ok(size)
    }

    fn validate_reference(
        vaddr: M::VirtAddr,
        reference: &H::MappingReference,
        size: PageSize,
    ) -> PtResult<PhysAddr> {
        let address = vaddr.into();
        let paddr = H::mapping_reference_paddr(reference);
        if H::mapping_reference_bytes(reference) != size as usize
            || !size.is_aligned(address)
            || !paddr.is_aligned(size)
        {
            return Err(PtError::NotAligned);
        }
        let last_va = address
            .checked_add(size as usize - 1)
            .ok_or(PtError::InvalidAddress)?;
        let last_pa = paddr
            .as_usize()
            .checked_add(size as usize - 1)
            .ok_or(PtError::InvalidAddress)?;
        if !M::vaddr_is_valid(address)
            || !M::vaddr_is_valid(last_va)
            || !M::paddr_is_valid(paddr.as_usize())
            || !M::paddr_is_valid(last_pa)
        {
            return Err(PtError::InvalidAddress);
        }
        Ok(paddr)
    }

    /// Replaces a matching leaf and its data hold as one mapping transaction.
    ///
    /// The new hold is retained before PTE mutation. A compare failure leaves
    /// the previous hold untouched; success retires the old hold until invalidation
    /// is confirmed. Empty flags clear the old leaf and release the unpublished
    /// new hold. The replacement must preserve the leaf size.
    ///
    /// # Errors
    /// Returns Changed on a stale snapshot or PageTable for invalid size/address.
    pub fn replace_if_same(
        &mut self,
        vaddr: M::VirtAddr,
        expected: PteSnapshot,
        reference: H::MappingReference,
        flags: PagingFlags,
    ) -> Result<PteSnapshot, PteReplaceError> {
        let size = expected.page_size();
        let paddr = Self::validate_reference(vaddr, &reference, size)?;
        let (entry, actual_size) = match self.get_entry_mut(vaddr) {
            Ok(value) => value,
            Err(PtError::NotMapped) => return Err(PteReplaceError::Changed { current: None }),
            Err(error) => return Err(error.into()),
        };
        if !entry.is_present() {
            return Err(PteReplaceError::Changed { current: None });
        }
        let current = PteSnapshot::from_entry(entry, actual_size);
        if actual_size != size || !expected.matches_entry(entry) {
            return Err(PteReplaceError::Changed {
                current: Some(current),
            });
        }
        let old_entry = *entry;
        if flags.is_empty() {
            entry.clear();
            // No new leaf owns the incoming hold; it remains a local owner.
            drop(reference);
        } else {
            let mut replacement = PTE::new_page(paddr, flags, size.is_huge());
            replacement.set_managed(true);
            let _transfer = H::mapping_reference_into_raw(reference);
            *entry = replacement;
        }
        self.flush_leaf(vaddr, size);
        self.retire_mapping_reference(old_entry, size);
        Ok(current)
    }

    /// Updates an externally owned mapping using its raw physical address.
    ///
    /// The virtual address must already be mapped. The page size is preserved
    /// from the existing mapping. A managed leaf can keep its physical range
    /// and change permissions; changing that range requires
    /// [`replace_if_same`](Self::replace_if_same) with a new hold.
    /// Empty flags clear the leaf and retire its hold after invalidation.
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
    /// - [`PtError::InvalidAddress`] — an address is out of range, the subtree
    ///   is borrowed, or a managed leaf would change its normalized PA without
    ///   supplying a new hold through replace_if_same.
    pub fn remap_raw(
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
        let paddr = paddr.align_down(size);
        if entry.is_managed() && entry.paddr() != paddr {
            return Err(PtError::InvalidAddress);
        }
        let old_entry = *entry;
        if flags.is_empty() {
            entry.clear();
        } else {
            entry.set_paddr(paddr);
            entry.set_flags(flags, size.is_huge());
        }
        self.flush_leaf(vaddr, size);
        if flags.is_empty() {
            self.retire_mapping_reference(old_entry, size);
        }
        Ok(size)
    }

    /// Replaces a present mapping only if the leaf entry still equals
    /// `expected`.
    ///
    /// This address-only commit does not acquire a mapping hold. A raw backing
    /// owner remains responsible for retirement; owned replacement uses
    /// [`replace_if_same`](Self::replace_if_same).
    ///
    /// This is the page-table commit primitive for raw COW-style transactions:
    /// higher layers may allocate or prepare a replacement page first, then
    /// commit the PTE only if the mapping observed before preparation has not
    /// changed. A changed PTE is reported without overwriting the current
    /// mapping, so callers can drop prepared resources and retry.
    ///
    /// The replacement preserves the page size from `expected` and normalizes
    /// the PA to that size. A managed leaf rejects a different physical range
    /// without a replacement hold. Empty flags clear the leaf and retire its hold
    /// after invalidation. A stale snapshot leaves both PTE and hold unchanged.
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
    pub fn replace_if_same_raw(
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

        let paddr = paddr.align_down(size);
        if entry.is_managed() && entry.paddr() != paddr {
            return Err(PteReplaceError::PageTable(PtError::InvalidAddress));
        }
        let old_entry = *entry;
        if flags.is_empty() {
            entry.clear();
        } else {
            entry.set_paddr(paddr);
            entry.set_flags(flags, size.is_huge());
        }
        self.flush_leaf(vaddr, size);
        if flags.is_empty() {
            self.retire_mapping_reference(old_entry, size);
        }
        Ok(current)
    }

    /// Conditionally replaces a managed leaf with externally owned raw backing.
    ///
    /// Unlike replace_if_same_raw, this explicitly ends the old managed mapping
    /// while transferring no lifetime hold for the new target. The old hold is
    /// retired through the existing completion/retention protocol. Success is a
    /// committed replacement even if an automatic retirement drain is unconfirmed;
    /// no ordinary error is returned after target publication.
    ///
    /// # Safety
    /// The caller must own the aligned target allocation for the entire leaf
    /// range, prepare its object-slot commit before calling, and keep it alive
    /// through all subsequent raw mapping use and required TLB retirement. After
    /// success the caller must not free the target as an unpublished candidate.
    /// Content initialization, access permissions and COW isolation are caller
    /// obligations separate from physical ownership.
    ///
    /// # Errors
    /// Returns Changed without mutation if the exact snapshot/size no longer
    /// matches, or PageTable for invalid addresses, misalignment, a raw source,
    /// no target access permissions or a borrowed subtree. Neither error consumes
    /// the external target or the source hold.
    pub unsafe fn replace_managed_with_external_if_same(
        &mut self,
        vaddr: M::VirtAddr,
        expected: PteSnapshot,
        paddr: PhysAddr,
        flags: PagingFlags,
    ) -> Result<PteSnapshot, PteReplaceError> {
        let size = expected.page_size();
        let address: usize = vaddr.into();
        if !size.is_aligned(address) || !paddr.is_aligned(size) {
            return Err(PtError::NotAligned.into());
        }
        let last_va = address
            .checked_add(size as usize - 1)
            .ok_or(PtError::InvalidAddress)?;
        let last_pa = paddr
            .as_usize()
            .checked_add(size as usize - 1)
            .ok_or(PtError::InvalidAddress)?;
        if !M::vaddr_is_valid(address)
            || !M::vaddr_is_valid(last_va)
            || !M::paddr_is_valid(paddr.as_usize())
            || !M::paddr_is_valid(last_pa)
            || !flags.intersects(PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE)
        {
            return Err(PtError::InvalidAddress.into());
        }
        let (entry, actual_size) = match self.get_entry_mut(vaddr) {
            Ok(value) => value,
            Err(PtError::NotMapped) => return Err(PteReplaceError::Changed { current: None }),
            Err(error) => return Err(error.into()),
        };
        if !entry.is_present() {
            return Err(PteReplaceError::Changed { current: None });
        }
        let current = PteSnapshot::from_entry(entry, actual_size);
        if actual_size != size || !expected.matches_entry(entry) {
            return Err(PteReplaceError::Changed {
                current: Some(current),
            });
        }
        if !entry.is_managed() {
            return Err(PtError::InvalidAddress.into());
        }
        let old_entry = *entry;
        // new_page starts raw; retaining the old managed bit would falsely
        // claim that the external target supplied a recoverable lifetime hold.
        *entry = PTE::new_page(paddr, flags, size.is_huge());
        self.flush_leaf(vaddr, size);
        self.retire_mapping_reference(old_entry, size);
        Ok(current)
    }

    /// Changes the permission flags of an existing mapping.
    ///
    /// The virtual address must already be mapped (present). The page size
    /// and physical address are preserved. Empty flags clear the leaf and
    /// retire its managed hold after invalidation; an object needing later
    /// fault rematerialization must keep its own backing reference.
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
        let old_entry = *entry;
        if flags.is_empty() {
            entry.clear();
        } else {
            entry.set_flags(flags, size.is_huge());
        }
        self.flush_leaf(vaddr, size);
        if flags.is_empty() {
            self.retire_mapping_reference(old_entry, size);
        }
        Ok(size)
    }

    /// Unmaps a virtual address, clearing the leaf page table entry.
    ///
    /// # Returns
    ///
    /// The previous `(physical_address, flags, page_size)` of the unmapped entry.
    /// This tuple transfers no ownership. Managed data holds remain in the
    /// retirement batch until invalidation; raw backing remains caller-owned.
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
        let old_entry = *entry;
        entry.clear();
        self.flush_leaf(vaddr, size);
        self.retire_mapping_reference(old_entry, size);
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
    pub fn map_region_raw(
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
            self.map_raw(v_addr, p_addr, p_size, flags)?;

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

    /// Unmaps present leaves of `page_size` in a possibly sparse region.
    ///
    /// Missing subtrees are skipped at their coverage boundary. Allocated leaf
    /// tables are scanned in place, without restarting a root walk for each VA.
    /// No table is allocated or freed. Cleared leaves use the same deferred TLB
    /// invalidation as [`unmap`](Self::unmap); backing frames must remain owned
    /// until [`finish`](Self::finish) returns.
    ///
    /// Both `vaddr` and `size` must be aligned to `page_size`. An aligned empty
    /// region at a valid address is a no-op. Nonempty regions must not overflow
    /// or cross the address range covered by one root table. Present leaves of
    /// a different size are rejected; huge mappings are never split or cleared
    /// beyond the requested range.
    ///
    /// # Errors
    ///
    /// - [`PtError::NotAligned`] — address or size is not page-size aligned.
    /// - [`PtError::InvalidAddress`] — invalid, overflowing or root-crossing range.
    /// - [`PtError::MappedToHugePage`] — a present leaf has a different size.
    ///
    /// Range validation precedes mutation. A leaf-size mismatch encountered
    /// during traversal may leave an already-cleared prefix, whose pending TLB
    /// invalidations are still owned by this guard. Callers must retain backing
    /// ownership on error; this operation does not roll back cleared PTEs.
    pub fn unmap_sparse_region(
        &mut self,
        vaddr: M::VirtAddr,
        size: usize,
        page_size: PageSize,
    ) -> PtResult {
        let start: usize = vaddr.into();
        if !page_size.is_aligned(start) || !page_size.is_aligned(size) {
            return Err(PtError::NotAligned);
        }
        if !M::vaddr_is_valid(start) {
            return Err(PtError::InvalidAddress);
        }
        if size == 0 {
            return Ok(());
        }
        let end = start.checked_add(size).ok_or(PtError::InvalidAddress)?;
        let address_bits = 12 + 9 * M::LEVELS;
        if !M::vaddr_is_valid(end - 1) || start >> address_bits != (end - 1) >> address_bits {
            return Err(PtError::InvalidAddress);
        }
        let table = self.table_of_mut(self.inner.root_paddr);
        self.unmap_sparse_table(table, address_bits - 9, start, end, page_size)
    }

    fn unmap_sparse_table(
        &mut self,
        table: &mut [PTE],
        shift: usize,
        mut start: usize,
        end: usize,
        page_size: PageSize,
    ) -> PtResult {
        let span = 1usize << shift;
        while start < end {
            // Clip before adding, including for the last high-half table.
            let next = start + (span - (start & (span - 1))).min(end - start);
            let entry = &mut table[(start >> shift) & (ENTRY_COUNT - 1)];
            #[cfg(unittest)]
            {
                self.sparse_entries_visited += 1;
            }
            // Directory entries need not carry a leaf's present bit (LoongArch).
            if !entry.is_unused() {
                // Reject borrowing before constructing any mutable child view.
                self.ensure_mutable_subtree(start.into())?;
                if shift == 12 || entry.is_huge() {
                    if !entry.is_present() {
                        start = next;
                        continue;
                    }
                    if span != page_size as usize {
                        return Err(PtError::MappedToHugePage);
                    }
                    let old_entry = *entry;
                    entry.clear();
                    self.flush_leaf(start.into(), page_size);
                    self.retire_mapping_reference(old_entry, page_size);
                } else if entry.is_table() {
                    match self.next_table_mut(entry) {
                        Ok(child) => {
                            self.unmap_sparse_table(child, shift - 9, start, next, page_size)?;
                        }
                        Err(PtError::NotMapped) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
            start = next;
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
    /// dropped first, the borrowed entries will point to freed frames. Source
    /// managed data holds are borrowed with those entries, never duplicated.
    /// Borrowers may query but cannot mutate shared subtrees; modify them through
    /// the source, invalidating every borrower (for example a kernel broadcast).
    /// The destination must be inactive when replacing root entries. Existing
    /// managed destination leaves in those subtrees must first be unmapped;
    /// pending managed retirements must already have completed. Before
    /// overwriting any target-owned subtree, the caller must end its hardware
    /// use and complete translation retirement; this operation reclaims that
    /// subtree immediately. Fresh inactive target tables need no retirement.
    ///
    /// # Availability
    ///
    /// Only available with `feature = "copy-from"`.
    #[cfg(feature = "copy-from")]
    pub fn copy_from(&mut self, other: &PageTable64<M, PTE, H>, start: M::VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        assert!(
            self.retired_mapping_refs.is_empty(),
            "managed mappings must be fully retired before borrowing a subtree"
        );
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
        assert!(
            (start_idx..end_idx).all(|index| {
                self.inner.borrowed_entries.get(index)
                    || !self.inner.entry_has_managed_leaf(&dst_table[index], 0)
            }),
            "managed mappings must be unmapped before borrowing a subtree"
        );
        for i in start_idx..end_idx {
            let entry = &mut dst_table[i];
            if !self.inner.borrowed_entries.set(i, true) && self.next_table(entry).is_ok() {
                self.dealloc_tree(entry.paddr(), 1);
            }
            *entry = src_table[i];
        }
    }

    /// Advances automatic retirement for the pending page-table changes.
    ///
    /// Managed holds are released only after confirmed invalidation. If the
    /// backend cannot complete, the guard retains those holds and a full retry.
    /// The receipt describes pending work, not completion; callers releasing
    /// their own managed owners do not have to inspect a completion flag.
    ///
    /// Automatic retention covers only holds transferred through `map`,
    /// `map_sized` or `replace_if_same`. Keeping a `PageRef` beside a
    /// raw mapping does not register a mapping hold with this guard.
    ///
    /// Use try_finish when permission revocation, raw backing release or another
    /// synchronous operation must complete before the caller proceeds. Its error
    /// requires the caller to retain raw backing and withhold completion. Drop
    /// and bounded retirement drains also advance automatic retirement.
    pub fn finish(&mut self) -> TlbFlushReceipt {
        let receipt = TlbFlushReceipt::new(self.flush.has_pending());
        // The guard owns the retry and all retained holds on error. Automatic
        // callers do not consume this internal completion result.
        let _ = self.flush_pending();
        receipt
    }

    /// Completes pending invalidation before allowing a synchronous operation
    /// to proceed. Success is a completion boundary, including automatic drains
    /// already performed by this guard; failure retains a full retry and all
    /// managed holds still awaiting retirement.
    ///
    /// # Errors
    /// Returns the backend's completion error when remote services are not
    /// ready, a nested request is deferred, a target times out, or the provider
    /// cannot establish the requested scope. The caller must not claim that
    /// permissions were revoked or release raw backing on error.
    pub fn try_finish(&mut self) -> Result<TlbFlushReceipt, crate::TlbFlushError> {
        let receipt = TlbFlushReceipt::new(self.flush.has_pending());
        self.flush_pending()?;
        Ok(receipt)
    }

    /// Detaches the pending invalidation of a kernel page-table change.
    ///
    /// The PTE writes are already visible; the caller must drop this guard and
    /// release the lock that protects the table before completing the returned
    /// handle. That lets CPUs competing for the same lock answer the shootdown
    /// IPI instead of spinning with interrupts disabled.
    ///
    /// Returns `None` when nothing is pending, or when the pending work cannot
    /// be detached safely: user tables use residency-scoped flushes, fresh
    /// mappings still need their publication fence, and managed retirements
    /// must stay with the guard until their invalidation completes.
    #[must_use = "the caller must complete the invalidation after unlocking"]
    pub fn defer_kernel_invalidation(&mut self) -> Option<DeferredKernelTlbFlush> {
        if !self.inner.is_kernel || self.fresh_map || !self.retired_mapping_refs.is_empty() {
            return None;
        }
        let scope = match core::mem::replace(&mut self.flush, ToFlush::None) {
            ToFlush::None => return None,
            ToFlush::Full => DeferredKernelTlbFlushScope::Full,
            ToFlush::Addresses(addresses) => DeferredKernelTlbFlushScope::Addresses(
                addresses.iter().map(|address| (*address).into()).collect(),
            ),
        };
        Some(DeferredKernelTlbFlush {
            scope: Some(scope),
            flush_all_cpus: |vaddr| {
                let vaddr = vaddr.map(M::VirtAddr::from);
                M::flush_tlb_all_cpus(vaddr)
            },
        })
    }

    #[cfg(not(docsrs))]
    fn invalidate_pending(&self) -> TlbFlushResult {
        if self.inner.is_kernel {
            return match &self.flush {
                ToFlush::None => Ok(()),
                ToFlush::Addresses(addrs) => addrs
                    .iter()
                    .try_for_each(|vaddr| M::flush_tlb_all_cpus(Some(*vaddr))),
                ToFlush::Full => M::flush_tlb_all_cpus(None),
            };
        }
        #[cfg(target_arch = "aarch64")]
        let asid = self.current_user_asid();
        #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
        let target_mask = self.current_user_cpu_mask();
        let invalidate = |vaddr| {
            #[cfg(target_arch = "aarch64")]
            {
                M::flush_tlb_process_asid(vaddr, asid)
            }
            #[cfg(all(feature = "smp", not(target_arch = "aarch64")))]
            {
                M::flush_tlb_process_mask(vaddr, target_mask)
            }
            #[cfg(not(any(target_arch = "aarch64", feature = "smp")))]
            {
                M::flush_tlb_process(vaddr)
            }
        };
        match &self.flush {
            ToFlush::None => Ok(()),
            ToFlush::Addresses(addrs) => {
                addrs.iter().try_for_each(|vaddr| invalidate(Some(*vaddr)))
            }
            ToFlush::Full => invalidate(None),
        }
    }

    fn flush_pending(&mut self) -> TlbFlushResult {
        let had_pending = self.flush.has_pending();
        #[cfg(not(docsrs))]
        let completion = self.invalidate_pending();
        #[cfg(docsrs)]
        let completion: TlbFlushResult = Ok(());
        // A successful invalidation supplies the publication barrier. A failed
        // hook may not have issued one, so publish fresh entries independently.
        if self.fresh_map && (!had_pending || completion.is_err()) {
            M::fence_fresh_map();
        }
        self.fresh_map = false;
        if completion.is_ok() {
            self.flush = ToFlush::None;
            self.retired_mapping_refs.clear();
        } else {
            // A prefix of addresses or CPUs may have completed. Retrying the
            // full scope is required before any remaining hold can end.
            self.flush = ToFlush::Full;
        }
        completion
    }
}

impl<M: PagingMetaData, PTE: PageTableEntry, H: PagingHandler> Drop
    for PageTableMut<'_, M, PTE, H>
{
    fn drop(&mut self) {
        if self.flush_pending().is_err() {
            self.retired_mapping_refs.forget_holds();
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/table64.rs");

unittest::path_mod_test!(tests_owned, "../ut-tests/owned_mappings.rs");
