// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page table definitions and traits.

use core::fmt;

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
/// - `paddr()` for a non-present entry may return an invalid address; callers
///   must check `is_present()` before using `paddr()`.
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

    /// Clears this entry, setting it to `EMPTY`.
    #[inline]
    fn clear(&mut self) {
        *self = Self::EMPTY;
    }
}

/// Page table operation errors.
#[derive(Debug, PartialEq, Clone, Copy)]
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
pub trait PagingMetaData: Sync + Send {
    /// Number of page table levels (3 or 4).
    const LEVELS: usize;

    /// Maximum number of physical address bits.
    const PA_MAX_BITS: usize;

    /// Maximum number of virtual address bits.
    const VA_MAX_BITS: usize;

    /// Maximum valid physical address.
    const PA_MAX_ADDR: usize = (1 << Self::PA_MAX_BITS) - 1;

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
    /// only the entry mapping `vaddr`.
    fn flush_tlb(vaddr: Option<Self::VirtAddr>);

    /// Flush TLB on CPUs where the **current task** has been scheduled
    /// (per-process CPU residency mask).  Correct for user page tables.
    ///
    /// Default: local-only, backward compatible with single-CPU builds.
    /// When `feature = "smp"` is enabled, architectures override this to
    /// also broadcast to remote CPUs.
    #[inline]
    fn flush_tlb_process(vaddr: Option<Self::VirtAddr>) {
        Self::flush_tlb(vaddr);
    }

    /// Like [`flush_tlb_process`](Self::flush_tlb_process), but scoped to
    /// `asid` when the architecture supports ASID-tagged TLB entries.
    ///
    /// Default: ignores `asid` and delegates to `flush_tlb_process`.
    #[inline]
    fn flush_tlb_process_asid(vaddr: Option<Self::VirtAddr>, _asid: u16) {
        Self::flush_tlb_process(vaddr);
    }

    /// Flush TLB on **all** online CPUs, regardless of task residency.
    /// Required for kernel page table modifications whose mappings are
    /// shared across every process.
    ///
    /// Default: same as [`flush_tlb_process`](Self::flush_tlb_process).
    /// Architectures without hardware broadcast (x86_64, riscv64,
    /// loongarch64) override this to broadcast to all online CPUs via IPI.
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<Self::VirtAddr>) {
        Self::flush_tlb_process(vaddr);
    }
}

/// Interface for broadcasting TLB flush to remote CPUs.
///
/// Implemented by the IPI subsystem (e.g. `kipi::tlb`) at link time via
/// `crate_interface::impl_interface`. This indirection breaks the
/// circular dependency between `page_table` and the IPI crate.
#[cfg(feature = "smp")]
#[crate_interface::def_interface]
pub trait TlbFlushIf {
    /// Flush TLB entries on remote CPUs where the **current task** has been
    /// scheduled (per-process CPU residency mask).
    ///
    /// This is the right choice for per-process user page tables, where only
    /// CPUs that have run threads of the current process can hold stale
    /// entries for this address space.
    ///
    /// If `vaddr` is `None`, flush the entire TLB; otherwise flush only the
    /// entry mapping `vaddr`.  The local CPU flush is the caller's
    /// responsibility — this method only handles remote CPUs.
    fn flush_process(vaddr: Option<VirtAddr>);

    /// Flush TLB entries on **all** online CPUs, regardless of task
    /// residency.  This is required for modifications to the global kernel
    /// page table, which is shared across every process — every online CPU
    /// may hold stale kernel TLB entries.
    ///
    /// If `vaddr` is `None`, flushes the entire TLB; otherwise flushes only the
    /// entry mapping `vaddr`. The local CPU flush is the caller's
    /// responsibility — this method only handles remote CPUs.
    fn flush_all_cpus(vaddr: Option<VirtAddr>);
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
    /// Allocates a 4K-aligned physical frame.
    ///
    /// # Returns
    ///
    /// `Some(paddr)` on success, `None` if no frames are available.
    fn alloc_frame() -> Option<PhysAddr>;

    /// Deallocates a previously allocated physical frame.
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
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
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

#[cfg(unittest)]
mod tests_page_table_defs {
    use memaddr::VirtAddr;
    use unittest::def_test;

    use super::{PageSize, PagingFlags, PagingMetaData};

    struct DummyMeta;

    impl PagingMetaData for DummyMeta {
        type VirtAddr = VirtAddr;

        const LEVELS: usize = 4;
        const PA_MAX_BITS: usize = 36;
        const VA_MAX_BITS: usize = 39;

        fn flush_tlb(_vaddr: Option<Self::VirtAddr>) {}
    }

    #[def_test]
    fn test_paging_flags_bits() {
        let flags = PagingFlags::READ | PagingFlags::WRITE;
        assert!(flags.contains(PagingFlags::READ));
        assert!(flags.contains(PagingFlags::WRITE));
        assert!(!flags.contains(PagingFlags::EXECUTE));
    }

    #[def_test]
    fn test_page_size_alignment() {
        assert!(PageSize::Size4K.is_aligned(0x2000));
        assert!(!PageSize::Size4K.is_aligned(0x2001));
        assert!(PageSize::Size2M.is_huge());
    }

    #[def_test]
    fn test_paging_metadata_bounds() {
        assert!(DummyMeta::paddr_is_valid(0));
        assert!(DummyMeta::paddr_is_valid((1 << DummyMeta::PA_MAX_BITS) - 1));
        assert!(!DummyMeta::paddr_is_valid(1 << DummyMeta::PA_MAX_BITS));
    }
}
