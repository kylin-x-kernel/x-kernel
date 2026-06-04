// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 page table entry and paging metadata.
//!
//! Implements [`PageTableEntry`] for LoongArch64 4-level paging using the
//! [`LaFlags`] attribute format. Also provides [`LA64MetaData`] with the
//! `PWCL`/`PWCH` register values for hardware page table walker configuration.
//!
//! # Type aliases
//!
//! - [`LA64PageTable`] — `PageTable64<LA64MetaData, La64PageEntry, H>`
//! - [`LA64PageTableMut`] — `PageTableMut<LA64MetaData, La64PageEntry, H>`

use memaddr::{PhysAddr, VirtAddr};

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

bitflags::bitflags! {
    #[derive(Debug)]
    pub(crate) struct LaFlags: u64 {
        const V = 1 << 0;
        const D = 1 << 1;
        const PLVL = 1 << 2;
        const PLVH = 1 << 3;
        const MATL = 1 << 4;
        const MATH = 1 << 5;
        const GH = 1 << 6;
        const P = 1 << 7;
        const W = 1 << 8;
        const G = 1 << 12;
        const NR = 1 << 61;
        const NX = 1 << 62;
        const RPLV = 1 << 63;
    }
}

impl From<LaFlags> for PagingFlags {
    fn from(f: LaFlags) -> Self {
        if !f.contains(LaFlags::V) {
            return Self::empty();
        }
        let mut ret = Self::empty();
        if !f.contains(LaFlags::NR) {
            ret |= Self::READ;
        }
        if f.contains(LaFlags::W) {
            ret |= Self::WRITE;
        }
        if !f.contains(LaFlags::NX) {
            ret |= Self::EXECUTE;
        }
        if f.contains(LaFlags::PLVL | LaFlags::PLVH) {
            ret |= Self::USER;
        }
        if !f.contains(LaFlags::MATL) {
            if f.contains(LaFlags::MATH) {
                ret |= Self::UNCACHED;
            } else {
                ret |= Self::DEVICE;
            }
        }
        ret
    }
}

impl From<PagingFlags> for LaFlags {
    fn from(f: PagingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::V | Self::P;
        if !f.contains(PagingFlags::READ) {
            ret |= Self::NR;
        }
        if f.contains(PagingFlags::WRITE) {
            ret |= Self::W | Self::D;
        }
        if !f.contains(PagingFlags::EXECUTE) {
            ret |= Self::NX;
        }
        if f.contains(PagingFlags::USER) {
            ret |= Self::PLVL | Self::PLVH;
        }
        if f.contains(PagingFlags::DEVICE) {
        } else if f.contains(PagingFlags::UNCACHED) {
            ret |= Self::MATH;
        } else {
            ret |= Self::MATL;
        }
        ret
    }
}

/// LoongArch64 page table entry (PTE).
///
/// A `#[repr(transparent)]` wrapper around `u64` that encodes physical
/// addresses and [`LaFlags`] attributes. Physical addresses are masked
/// to cover bits 12–47 (48-bit physical address space).
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct La64PageEntry(u64);

impl La64PageEntry {
    const PADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
}

impl PageTableEntry for La64PageEntry {
    crate::impl_pte_common_ops!(LaFlags, Self::PADDR_MASK);

    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut f = LaFlags::from(flags);
        if is_huge {
            f |= LaFlags::GH;
        }
        Self(f.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        Self(paddr.as_usize() as u64 & Self::PADDR_MASK)
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        let mut f = LaFlags::from(flags);
        if is_huge {
            f |= LaFlags::GH;
        }
        self.0 = (self.0 & Self::PADDR_MASK) | f.bits();
    }

    fn is_present(&self) -> bool {
        LaFlags::from_bits_truncate(self.0).contains(LaFlags::V)
    }

    fn is_huge(&self) -> bool {
        LaFlags::from_bits_truncate(self.0).contains(LaFlags::GH)
    }
}

crate::impl_pte_debug!(La64PageEntry);

/// LoongArch64 paging metadata: 4-level paging, 48-bit PA, 48-bit VA.
///
/// Provides `PWCH_VALUE` and `PWCL_VALUE` constants for configuring the
/// hardware page table walker registers (`PWCH` / `PWCL`).
pub struct LA64MetaData;

impl LA64MetaData {
    pub const PWCH_VALUE: u32 = 39 | (9 << 6);
    pub const PWCL_VALUE: u32 = 12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25);
}

impl PagingMetaData for LA64MetaData {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 48;
    const VA_MAX_BITS: usize = 48;

    fn flush_tlb(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_process(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
        crate_interface::call_interface!(crate::defs::TlbFlushIf::flush_process(vaddr));
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
        crate_interface::call_interface!(crate::defs::TlbFlushIf::flush_all_cpus(vaddr));
    }
}

/// LoongArch64 page table type alias.
pub type LA64PageTable<H> = PageTable64<LA64MetaData, La64PageEntry, H>;
/// LoongArch64 mutable page table type alias.
pub type LA64PageTableMut<'a, H> = PageTableMut<'a, LA64MetaData, La64PageEntry, H>;
