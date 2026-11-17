// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page table definitions and traits.

use core::fmt;

#[cfg(feature = "smp")]
use kcpu_id_map::KCpuMask;
use memaddr::{MemoryAddr, PhysAddr, VirtAddr};

bitflags::bitflags! {
    /// Page table entry permission and attribute flags.
    ///
    /// These flags are architecture-independent. Each architecture's PTE
    /// implementation converts between `PagingFlags` and its native
    /// flag format (e.g., x86_64 `PageTableFlags`, AArch64 `Arm64Attr`).
    #[derive(Clone, Copy, PartialEq)]
    pub struct PagingFlags: usize {
        const READ          = 1 << 0;
        const WRITE         = 1 << 1;
        const EXECUTE       = 1 << 2;
        const USER          = 1 << 3;
        const DEVICE        = 1 << 4;
        const UNCACHED      = 1 << 5;
        const SHARED        = 1 << 6;
    }
}

impl fmt::Debug for PagingFlags {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// Trait implemented by architecture-specific page table entries.
///
/// Each architecture provides a `#[repr(transparent)]` wrapper around `u64`
/// that implements this trait, encoding/decoding physical addresses and
/// permission flags in the format required by the hardware.
///
/// # Invariants
///
/// - `EMPTY` represents an unused entry (all bits zero).
/// - `is_present()` returns `false` for `EMPTY` entries.
/// - Before using `paddr()`, callers must establish a present leaf or a
///   table pointer via `is_table()`; a non-present leaf is not a table.
/// - `new_page()` and `new_table()` start without a managed category.
/// - `set_paddr()` and `set_flags()` preserve the software ownership category;
///   only `set_managed()` or clearing the whole entry changes it.
/// - The managed category affects neither the physical address nor public
///   permissions, and setting its bit alone does not acquire a lifetime hold.
pub trait PageTableEntry: fmt::Debug + Clone + Copy + Sync + Send + Sized {
    /// The sentinel value for an unused entry (all bits zero).
    const EMPTY: Self;

    /// Creates a page mapping entry.
    ///
    /// # Arguments
    ///
    /// - `paddr` — physical address of the target page (must be aligned to the page size).
    /// - `flags` — permission and attribute flags.
    /// - `is_huge` — whether this is a huge/superpage entry.
    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self;

    /// Creates a page-table pointer entry (points to the next-level table).
    ///
    /// # Arguments
    ///
    /// - `paddr` — physical address of the next-level page table frame.
    fn new_table(paddr: PhysAddr) -> Self;

    /// Returns the physical address stored in this entry.
    fn paddr(&self) -> PhysAddr;

    /// Returns the permission and attribute flags of this entry.
    fn flags(&self) -> PagingFlags;

    /// Updates the physical address stored in this entry.
    fn set_paddr(&mut self, paddr: PhysAddr);

    /// Updates the permission and attribute flags of this entry.
    ///
    /// # Arguments
    ///
    /// - `flags` — new permission and attribute flags.
    /// - `is_huge` — whether this is a huge/superpage entry.
    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool);

    /// Returns whether this leaf owns one transferred mapping hold.
    /// This software category is independent of access permissions.
    fn is_managed(&self) -> bool;

    /// Sets the software ownership category of a leaf descriptor.
    /// Callers must transfer or recover its hold as part of the same transaction.
    fn set_managed(&mut self, is_managed: bool);

    /// Returns the raw bits of this entry.
    fn bits(self) -> usize;

    /// Returns `true` if this entry is unused (all bits zero).
    #[inline]
    fn is_unused(&self) -> bool {
        self.bits() == 0
    }

    /// Returns `true` if this entry is present (valid) in hardware terms.
    fn is_present(&self) -> bool;

    /// Returns `true` if this entry maps a huge/superpage.
    fn is_huge(&self) -> bool;

    /// Returns whether a non-leaf-level entry points to a next-level table.
    ///
    /// Only call this above the final page-table level. Architectures whose
    /// directories lack the present bit must override this classification.
    fn is_table(&self) -> bool {
        self.is_present() && !self.is_huge()
    }

    /// Clears this entry, setting it to `EMPTY`.
    #[inline]
    fn clear(&mut self) {
        *self = Self::EMPTY;
    }
}

/// Page table operation errors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PtError {
    /// Frame allocation failed (out of memory).
    NoMemory,
    /// Address or size is not properly aligned.
    NotAligned,
    /// The virtual address is not mapped.
    NotMapped,
    /// The virtual address is already mapped.
    AlreadyMapped,
    /// Cannot walk past a huge page entry.
    MappedToHugePage,
    /// Virtual or physical address is outside the valid range.
    InvalidAddress,
}

/// Stable snapshot of a present page-table leaf entry.
///
/// The snapshot is the compare token used by conditional replacement paths
/// such as COW write faults. Callers may inspect the mapping identity
/// and permissions, but only `page_table` interprets `raw_bits` when checking
/// whether the observed PTE is still unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PteSnapshot {
    paddr: PhysAddr,
    flags: PagingFlags,
    page_size: PageSize,
    raw_bits: usize,
}

impl PteSnapshot {
    /// Creates a snapshot from a present page-table entry.
    pub(crate) fn from_entry<PTE: PageTableEntry>(entry: &PTE, page_size: PageSize) -> Self {
        Self {
            paddr: entry.paddr(),
            flags: entry.flags(),
            page_size,
            raw_bits: entry.bits(),
        }
    }

    /// Returns the base physical address encoded in the leaf entry.
    pub const fn paddr(self) -> PhysAddr {
        self.paddr
    }

    /// Returns the decoded permission and attribute flags.
    pub const fn flags(self) -> PagingFlags {
        self.flags
    }

    /// Returns the page size represented by this leaf entry.
    pub const fn page_size(self) -> PageSize {
        self.page_size
    }

    /// Returns `true` when `entry` still matches this snapshot exactly.
    pub(crate) fn matches_entry<PTE: PageTableEntry>(self, entry: &PTE) -> bool {
        self.raw_bits == entry.bits()
    }
}

/// Error returned by conditional PTE replacement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PteReplaceError {
    /// Page-table walk, address validation, or replacement validation failed.
    PageTable(PtError),
    /// The current leaf entry no longer matches the expected snapshot.
    ///
    /// `current == None` means the address is no longer mapped by a present
    /// leaf entry. `Some(snapshot)` lets callers decide whether to retry,
    /// drop prepared resources, or reclassify the fault.
    Changed {
        /// Current present mapping, if any.
        current: Option<PteSnapshot>,
    },
}

impl From<PtError> for PteReplaceError {
    fn from(value: PtError) -> Self {
        Self::PageTable(value)
    }
}

/// Work observed at one automatic page-table retirement boundary.
///
/// This is diagnostic batching information, not a completion proof. Managed
/// holds remain owned internally until invalidation completes. Call
/// [`crate::PageTableMut::try_finish`] when synchronous invalidation is required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlbFlushReceipt {
    had_pending: bool,
}

impl TlbFlushReceipt {
    /// Records whether this boundary had pending invalidation work.
    pub(crate) const fn new(had_pending: bool) -> Self {
        Self { had_pending }
    }

    /// Returns whether this boundary had pending invalidation work.
    /// Intermediate automatic drains are not included.
    pub const fn had_pending(self) -> bool {
        self.had_pending
    }
}

/// Why a requested TLB invalidation could not complete synchronously.
///
/// These outcomes govern retirement inside the page-table layer. They do not
/// transfer or release a managed mapping hold. Ordinary managed callers rely on
/// automatic retention; synchronous operations receive an error from
/// [`crate::PageTableMut::try_finish`] and must not claim revocation completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlbFlushError {
    /// Remote CPU services cannot yet accept and acknowledge shootdowns.
    NotReady,
    /// A nested request must wait for its initiator's active request to finish.
    Deferred,
    /// At least one required target did not acknowledge before the wait ended.
    TimedOut,
    /// The metadata provider cannot establish the requested remote scope.
    UnsupportedScope,
}

/// Completion result at the architecture/shootdown retirement boundary.
///
/// `Ok(())` confirms the whole requested scope. Errors leave backing retirement
/// pending; local hardware invalidations that always complete return `()`.
pub type TlbFlushResult = Result<(), TlbFlushError>;

#[cfg(feature = "kerrno")]
impl From<PtError> for kerrno::KError {
    fn from(value: PtError) -> Self {
        match value {
            PtError::NoMemory => kerrno::KError::NoMemory,
            _ => kerrno::KError::InvalidInput,
        }
    }
}

/// Result type for page table operations.
pub type PtResult<T = ()> = Result<T, PtError>;

/// Architecture-specific paging metadata.
///
/// Provides compile-time constants for page table geometry and runtime
/// hooks for TLB management. Each architecture implements this trait
/// to describe its paging model.
///
/// # Safety
///
/// - `LEVELS` must be 3 or 4 (matching `walk_page_table!` macro support).
/// - `PA_MAX_BITS` and `VA_MAX_BITS` must match the hardware configuration.
/// - `flush_tlb()` must actually invalidate the relevant TLB entries.
/// - New table frames must be initialized with the architecture's required
///   ordering before a parent descriptor or root register exposes them to
///   page-table walkers. This obligation is independent of the fresh-map
///   invalidation policy; finalization hooks run after parent publication.
/// - `INVALID_TO_VALID_NEEDS_TLBI` may only be `false` when the architecture
///   guarantees that no translation for an invalid entry can be cached, and
///   `fence_fresh_map()` then orders the entry write against other
///   translation table walkers.
pub trait PagingMetaData: Sync + Send {
    /// Number of page table levels (3 or 4).
    const LEVELS: usize;

    /// Maximum number of physical address bits.
    const PA_MAX_BITS: usize;

    /// Maximum number of virtual address bits.
    const VA_MAX_BITS: usize;

    /// Maximum valid physical address.
    const PA_MAX_ADDR: usize = (1 << Self::PA_MAX_BITS) - 1;

    /// Whether making a previously invalid entry valid requires a TLB
    /// invalidation.
    ///
    /// Architectures whose walkers cannot hold a translation for an invalid
    /// entry may set this to `false`, so installing a fresh mapping skips the
    /// invalidation and only runs [`fence_fresh_map`](Self::fence_fresh_map).
    /// Architectures that may cache invalid entries must keep the default,
    /// otherwise a walker can keep using the stale invalid translation after
    /// the entry became valid. Keeping the default does not by itself establish
    /// initialization-before-publication ordering for newly allocated tables;
    /// that separate obligation depends on the architecture's walker protocol.
    const INVALID_TO_VALID_NEEDS_TLBI: bool = true;

    /// Virtual address type for this architecture.
    type VirtAddr: MemoryAddr;

    /// Returns `true` if the physical address is within the valid range.
    fn paddr_is_valid(paddr: usize) -> bool {
        paddr <= Self::PA_MAX_ADDR
    }

    /// Returns `true` if the virtual address has valid canonical form.
    ///
    /// For most architectures, the top bits must be all zeros or all ones
    /// (sign-extension of bit `VA_MAX_BITS - 1`).
    fn vaddr_is_valid(vaddr: usize) -> bool {
        let top_mask = usize::MAX << (Self::VA_MAX_BITS - 1);
        (vaddr & top_mask) == 0 || (vaddr & top_mask) == top_mask
    }

    /// Flushes TLB entry(ies) on the local CPU.
    ///
    /// If `vaddr` is `None`, flushes the entire TLB; otherwise flushes
    /// only the entry mapping `vaddr`. The hook must order preceding PTE writes
    /// and complete the requested invalidation before returning; managed data
    /// holds can be released immediately afterward.
    fn flush_tlb(vaddr: Option<Self::VirtAddr>);

    /// Orders freshly installed page-table entries against other translation
    /// table walkers.
    ///
    /// Runs once per finalization boundary in which entries were installed
    /// into previously unused slots without an invalidation, so the entry
    /// writes are observable before another PE walks them. Architectures that
    /// invalidate on a fresh mapping never reach this hook, because the
    /// invalidation sequence already orders the write.
    ///
    /// This hook runs after new parent descriptors may have been published. It
    /// cannot supply initialization-before-publication ordering for new table
    /// frames; any ordering required at that earlier boundary is separate.
    ///
    /// Default: no barrier.
    #[inline]
    fn fence_fresh_map() {}

    /// Invalidates translations for this user address space.
    ///
    /// `Ok(())` requires synchronous completion, including architectural
    /// barriers and any required remote acknowledgements. The default supports
    /// single-CPU use only; SMP providers must establish their full scope.
    ///
    /// # Errors
    /// Returns an error when completion is unavailable, deferred or timed out.
    /// The default returns UnsupportedScope in SMP builds.
    #[inline]
    fn flush_tlb_process(vaddr: Option<Self::VirtAddr>) -> TlbFlushResult {
        Self::flush_tlb(vaddr);
        if cfg!(feature = "smp") {
            Err(TlbFlushError::UnsupportedScope)
        } else {
            Ok(())
        }
    }

    /// Invalidates a user address space for an explicit residency mask.
    ///
    /// # Errors
    /// Returns an error unless the whole requested scope completed. The default
    /// supports an empty remote scope only; SMP architectures override it.
    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_process_mask(
        vaddr: Option<Self::VirtAddr>,
        target_mask: KCpuMask,
    ) -> TlbFlushResult {
        Self::flush_tlb(vaddr);
        if target_mask.is_empty() {
            Ok(())
        } else {
            Err(TlbFlushError::UnsupportedScope)
        }
    }

    /// Invalidates this address space's ASID when supported by the architecture.
    /// The default delegates to flush_tlb_process without narrowing its scope.
    ///
    /// # Errors
    /// Returns the delegated completion error; no error authorizes backing reuse.
    #[inline]
    fn flush_tlb_process_asid(vaddr: Option<Self::VirtAddr>, _asid: u16) -> TlbFlushResult {
        Self::flush_tlb_process(vaddr)
    }

    /// Invalidates kernel translations on all CPUs that can retain them.
    /// Software shootdown architectures override this hook for the all-CPU scope.
    ///
    /// # Errors
    /// Returns an error unless every relevant CPU completed invalidation. The
    /// default supports single-CPU use only.
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<Self::VirtAddr>) -> TlbFlushResult {
        Self::flush_tlb_process(vaddr)
    }
}

/// Interface for broadcasting TLB flush to remote CPUs.
///
/// Implemented by the IPI subsystem (e.g. `kipi::tlb`) at link time via
/// `kiface::provide`. This indirection breaks the
/// circular dependency between `page_table` and the IPI crate.
#[cfg(feature = "smp")]
#[kiface::interface]
pub trait TlbFlushIf {
    /// Flush TLB entries on remote CPUs that may still retain the target
    /// user address space's translations (mm-owned CPU residency mask).
    ///
    /// This is the right choice for per-process user page tables, where only
    /// CPUs that have run threads of the current process can hold stale
    /// entries for this address space.
    ///
    /// If `vaddr` is `None`, flush the entire TLB; otherwise flush only the
    /// entry mapping `vaddr`.  The local CPU flush is the caller's
    /// responsibility — this method only handles remote CPUs.
    ///
    /// # Errors
    /// Returns an error when remote completion is unavailable, deferred or timed
    /// out. Only Ok(()) confirms the requested scope.
    fn flush_process_mask(vaddr: Option<VirtAddr>, target_mask: KCpuMask) -> TlbFlushResult;

    /// Flush TLB entries on **all** online CPUs, regardless of task
    /// residency.  This is required for modifications to the global kernel
    /// page table, which is shared across every process — every online CPU
    /// may hold stale kernel TLB entries.
    ///
    /// If `vaddr` is `None`, flushes the entire TLB; otherwise flushes only the
    /// entry mapping `vaddr`. The local CPU flush is the caller's
    /// responsibility — this method only handles remote CPUs.
    ///
    /// # Errors
    /// Returns an error when remote completion is unavailable, deferred or timed
    /// out. Only Ok(()) confirms the requested scope.
    fn flush_all_cpus(vaddr: Option<VirtAddr>) -> TlbFlushResult;
}

/// Hooks for allocating and mapping page table frames.
///
/// The implementor provides physical frame allocation and the phys-to-virt
/// direct mapping used to access page table frames in memory.
///
/// # Safety
///
/// - `alloc_frame()` must return 4K-aligned physical addresses.
/// - `p2v()` must be an injective mapping: each physical address maps to
///   exactly one unique virtual address.
/// - The virtual address returned by `p2v()` must point to a valid,
///   4K-sized, read-write mapping of the physical frame.
pub trait PagingHandler: Sized {
    /// One owned lifetime hold for mapped data, separate from table storage.
    ///
    /// The value must keep its reported physical range allocated until Drop,
    /// and Drop must release only this hold. Its address and length must stay
    /// stable while held and recoverable from their exact raw range. Transferring
    /// and recovering it must not change its lifetime counts. Drop must not sleep
    /// in the mutation caller's context.
    /// This conveys no content access or device completion.
    /// Handlers supporting only raw mappings can use `core::convert::Infallible`.
    type MappingReference: Send + Sync;

    /// Returns the first physical byte protected by the supplied hold.
    fn mapping_reference_paddr(reference: &Self::MappingReference) -> PhysAddr;

    /// Returns the contiguous byte length selected by the supplied hold.
    /// Every byte in that range must remain backed until the hold ends.
    fn mapping_reference_bytes(reference: &Self::MappingReference) -> usize;

    /// Transfers a hold into its exact physical range without releasing it.
    /// The returned range must equal the address and byte length reported above.
    /// A managed leaf records this outstanding transfer until it is recovered.
    fn mapping_reference_into_raw(reference: Self::MappingReference) -> (PhysAddr, usize);

    /// Recovers one outstanding mapping hold without acquiring another reference.
    ///
    /// # Safety
    /// The exact pair must come from one unconsumed `mapping_reference_into_raw`
    /// transfer. The caller must own that transfer exclusively and recover it
    /// exactly once, keeping the resulting hold through required TLB retirement.
    /// A valid physical address or a metadata lookup alone cannot supply a hold.
    unsafe fn mapping_reference_from_raw(paddr: PhysAddr, bytes: usize) -> Self::MappingReference;

    /// Acquires an independent hold from a live managed leaf's raw transfer.
    ///
    /// The original transfer remains owned by its leaf, including on error.
    /// Raw-only providers reject this operation by default.
    ///
    /// # Safety
    /// The caller must hold exclusive page-table access and retain the live
    /// managed leaf owning this exact physical range and allocation incarnation
    /// throughout the call. Address validity or metadata lookup is insufficient.
    /// The implementation must not consume or release the source transfer.
    ///
    /// # Errors
    /// Returns InvalidAddress when unsupported, or NoMemory when another hold
    /// cannot be acquired. Failure leaves the original lifetime counts unchanged.
    unsafe fn mapping_reference_try_clone_from_raw(
        _paddr: PhysAddr,
        _bytes: usize,
    ) -> PtResult<Self::MappingReference> {
        Err(PtError::InvalidAddress)
    }

    /// Allocates a 4K-aligned physical frame.
    ///
    /// # Returns
    ///
    /// `Some(paddr)` on success, `None` if no frames are available.
    fn alloc_frame() -> Option<PhysAddr>;

    /// Consumes one storage ownership previously returned by alloc_frame.
    ///
    /// The caller must have ended hardware use and required translation
    /// retirement before release. A copied/borrowed table entry does not own
    /// another allocation and must not release its source table's frame.
    fn dealloc_frame(paddr: PhysAddr);

    /// Converts a physical address to a virtual address via the direct mapping.
    ///
    /// The returned virtual address must uniquely map to the given physical
    /// address and provide read-write access to the entire 4K frame.
    fn p2v(paddr: PhysAddr) -> VirtAddr;
}

/// Supported page sizes.
///
/// The discriminant values are the actual byte sizes of each page.
#[repr(usize)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageSize {
    /// 4 KiB page (standard granularity).
    Size4K = 0x1000,
    /// 2 MiB huge page.
    Size2M = 0x20_0000,
    /// 1 GiB huge page.
    Size1G = 0x4000_0000,
}

impl PageSize {
    /// Returns `true` if this is a huge page size (2M or 1G).
    pub const fn is_huge(self) -> bool {
        matches!(self, Self::Size1G | Self::Size2M)
    }

    /// Returns `true` if `addr_or_size` is aligned to this page size.
    pub const fn is_aligned(self, addr_or_size: usize) -> bool {
        memaddr::is_aligned(addr_or_size, self as usize)
    }

    /// Returns the offset of `addr` within a page of this size.
    pub const fn align_offset(self, addr: usize) -> usize {
        memaddr::align_offset(addr, self as usize)
    }
}

impl From<PageSize> for usize {
    fn from(size: PageSize) -> usize {
        size as usize
    }
}

unittest::path_mod_test!(tests_page_table_defs, "../ut-tests/defs.rs");
