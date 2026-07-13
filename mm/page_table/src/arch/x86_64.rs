// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 page table entry and paging metadata.
//!
//! Implements [`PageTableEntry`] for x86_64 long-mode 4-level paging with
//! optional AMD SEV C-bit encryption support. The C-bit position is read
//! from `kbuild_config::SEV_CBIT_POS` at compile time.
//!
//! # Type aliases
//!
//! - [`X64PageTable`] — `PageTable64<X64PagingMetaData, X64PageEntry, H>`
//! - [`X64PageTableMut`] — `PageTableMut<X64PagingMetaData, X64PageEntry, H>`

use memaddr::{PhysAddr, VirtAddr};
pub(crate) use x86_64::structures::paging::page_table::PageTableFlags as PTF;

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

const SEV_CBIT_MASK: Option<u64> = if kbuild_config::SEV_CBIT_POS == 0 {
    None
} else {
    Some(1u64 << kbuild_config::SEV_CBIT_POS)
};

#[derive(Clone, Copy)]
#[repr(transparent)]
struct EncodedPtePhys(u64);

impl EncodedPtePhys {
    #[inline]
    fn from_page(flags: PagingFlags, paddr: PhysAddr) -> Self {
        let cbit = if flags.contains(PagingFlags::SHARED) {
            0
        } else {
            SEV_CBIT_MASK.unwrap_or(0)
        };
        Self((paddr.as_usize() as u64 | cbit) & X64PageEntry::PADDR_MASK)
    }

    #[inline]
    fn from_table(paddr: PhysAddr) -> Self {
        Self((paddr.as_usize() as u64 | SEV_CBIT_MASK.unwrap_or(0)) & X64PageEntry::PADDR_MASK)
    }

    #[inline]
    fn from_raw(raw: u64) -> Self {
        Self(raw & X64PageEntry::PADDR_MASK)
    }

    #[inline]
    fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    fn paddr(self) -> PhysAddr {
        let paddr = self.0 & !SEV_CBIT_MASK.unwrap_or(0);
        PhysAddr::from((paddr & X64PageEntry::PADDR_MASK) as usize)
    }

    #[inline]
    fn is_shared(self) -> bool {
        SEV_CBIT_MASK.is_some_and(|mask| (self.0 & mask) == 0)
    }

    #[inline]
    fn with_paddr(self, paddr: PhysAddr) -> Self {
        Self::from_raw((paddr.as_usize() as u64) | (self.0 & SEV_CBIT_MASK.unwrap_or(0)))
    }
}

impl From<PTF> for PagingFlags {
    fn from(f: PTF) -> Self {
        if !f.contains(PTF::PRESENT) {
            return Self::empty();
        }
        let mut ret = Self::READ;
        if f.contains(PTF::WRITABLE) {
            ret |= Self::WRITE;
        }
        if !f.contains(PTF::NO_EXECUTE) {
            ret |= Self::EXECUTE;
        }
        if f.contains(PTF::USER_ACCESSIBLE) {
            ret |= Self::USER;
        }
        if f.contains(PTF::NO_CACHE) {
            ret |= Self::UNCACHED;
        }
        ret
    }
}

impl From<PagingFlags> for PTF {
    fn from(f: PagingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::PRESENT;
        if f.contains(PagingFlags::WRITE) {
            ret |= Self::WRITABLE;
        }
        if !f.contains(PagingFlags::EXECUTE) {
            ret |= Self::NO_EXECUTE;
        }
        if f.contains(PagingFlags::USER) {
            ret |= Self::USER_ACCESSIBLE;
        }
        if f.contains(PagingFlags::DEVICE) || f.contains(PagingFlags::UNCACHED) {
            ret |= Self::NO_CACHE | Self::WRITE_THROUGH;
        }
        ret
    }
}

/// x86_64 page table entry (PTE).
///
/// A `#[repr(transparent)]` wrapper around `u64` that encodes physical
/// addresses, permission flags, and the AMD SEV C-bit. The C-bit position
/// is determined by `kbuild_config::SEV_CBIT_POS`.
///
/// Physical addresses in the PTE are masked to cover bits 12–51
/// (52-bit physical address space). The C-bit is embedded within this
/// range when SEV is active.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X64PageEntry(u64);

impl X64PageEntry {
    const PADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
}

impl PageTableEntry for X64PageEntry {
    const EMPTY: Self = Self(0);

    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut f = PTF::from(flags);
        if is_huge {
            f |= PTF::HUGE_PAGE;
        }
        Self(f.bits() | EncodedPtePhys::from_page(flags, paddr).raw())
    }

    fn new_table(paddr: PhysAddr) -> Self {
        let f = PTF::PRESENT | PTF::WRITABLE | PTF::USER_ACCESSIBLE;
        Self(f.bits() | EncodedPtePhys::from_table(paddr).raw())
    }

    fn paddr(&self) -> PhysAddr {
        EncodedPtePhys::from_raw(self.0).paddr()
    }

    fn flags(&self) -> PagingFlags {
        let mut flags: PagingFlags = PTF::from_bits_truncate(self.0).into();
        if self.is_present() && EncodedPtePhys::from_raw(self.0).is_shared() {
            flags |= PagingFlags::SHARED;
        }
        flags
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        let encoded = EncodedPtePhys::from_raw(self.0).with_paddr(paddr);
        self.0 = (self.0 & !Self::PADDR_MASK) | encoded.raw();
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        let mut f = PTF::from(flags);
        if is_huge {
            f |= PTF::HUGE_PAGE;
        }
        let encoded = EncodedPtePhys::from_page(flags, EncodedPtePhys::from_raw(self.0).paddr());
        self.0 = f.bits() | encoded.raw()
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_present(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::PRESENT)
    }

    fn is_huge(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::HUGE_PAGE)
    }
}

crate::impl_pte_debug!(X64PageEntry);

/// x86_64 paging metadata: 4-level paging, 52-bit PA, 48-bit VA.
pub struct X64PagingMetaData;

impl PagingMetaData for X64PagingMetaData {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 52;
    const VA_MAX_BITS: usize = 48;

    #[inline]
    fn flush_tlb(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_process_mask(vaddr: Option<VirtAddr>, target_mask: kcpu_id_map::KCpuMask) {
        karch::flush_tlb(vaddr);
        crate::defs::TlbFlushIf::flush_process_mask(vaddr, target_mask);
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
        crate::defs::TlbFlushIf::flush_all_cpus(vaddr);
    }
}

/// x86_64 page table type alias.
pub type X64PageTable<H> = PageTable64<X64PagingMetaData, X64PageEntry, H>;
/// x86_64 mutable page table type alias.
pub type X64PageTableMut<'a, H> = PageTableMut<'a, X64PagingMetaData, X64PageEntry, H>;
