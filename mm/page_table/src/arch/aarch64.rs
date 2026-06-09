// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 page table entry and paging metadata.
//!
//! Implements [`PageTableEntry`] for AArch64 4-level paging using the
//! [`Arm64Attr`] attribute format. AArch64 TLB flush uses Inner Shareable
//! TLBI instructions, so no software IPI shootdown is needed for SMP.
//!
//! # Type aliases
//!
//! - [`A64PageTable`] — `PageTable64<A64PagingMetaData, A64PageEntry, H>`
//! - [`A64PageTableMut`] — `PageTableMut<A64PagingMetaData, A64PageEntry, H>`

use memaddr::{PhysAddr, VirtAddr};

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Arm64Attr: u64 {
        const VALID =       1 << 0;
        const NON_BLOCK =   1 << 1;
        const ATTR_INDX =   0b111 << 2;
        const NS =          1 << 5;
        const AP_EL0 =      1 << 6;
        const AP_RO =       1 << 7;
        const INNER =       1 << 8;
        const SHAREABLE =   1 << 9;
        const AF =          1 << 10;
        const NG =          1 << 11;
        const CONTIGUOUS =  1 <<  52;
        const PXN =         1 <<  53;
        const UXN =         1 <<  54;

        const PXN_TABLE =           1 << 59;
        const XN_TABLE =            1 << 60;
        const AP_NO_EL0_TABLE =     1 << 61;
        const AP_NO_WRITE_TABLE =   1 << 62;
        const NS_TABLE =            1 << 63;
    }
}

#[repr(u64)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Arm64MemAttr {
    Device             = 0,
    Normal             = 1,
    NormalNonCacheable = 2,
}

impl Arm64Attr {
    const ATTR_INDEX_MASK: u64 = 0b1_1100;

    pub const fn from_mem_attr(idx: Arm64MemAttr) -> Self {
        let mut bits = (idx as u64) << 2;
        if matches!(idx, Arm64MemAttr::Normal | Arm64MemAttr::NormalNonCacheable) {
            bits |= Self::INNER.bits() | Self::SHAREABLE.bits();
        }
        Self::from_bits_retain(bits)
    }

    pub(crate) const fn mem_attr(&self) -> Option<Arm64MemAttr> {
        let idx = (self.bits() & Self::ATTR_INDEX_MASK) >> 2;
        Some(match idx {
            0 => Arm64MemAttr::Device,
            1 => Arm64MemAttr::Normal,
            2 => Arm64MemAttr::NormalNonCacheable,
            _ => return None,
        })
    }
}

impl Arm64MemAttr {
    pub const MAIR_VALUE: u64 = Self::mair_el1_val();

    pub const fn mair_el1_val() -> u64 {
        let device = 0x00;
        let normal = 0xff;
        let normal_nc = 0x44;
        (device << (8 * Self::Device as u64))
            | (normal << (8 * Self::Normal as u64))
            | (normal_nc << (8 * Self::NormalNonCacheable as u64))
    }
}

impl From<Arm64Attr> for PagingFlags {
    fn from(a: Arm64Attr) -> Self {
        if !a.contains(Arm64Attr::VALID) {
            return Self::empty();
        }
        let mut f = Self::READ;
        if !a.contains(Arm64Attr::AP_RO) && !a.contains(Arm64Attr::AP_NO_WRITE_TABLE) {
            f |= Self::WRITE;
        }
        if !a.contains(Arm64Attr::UXN) && !a.contains(Arm64Attr::XN_TABLE) {
            f |= Self::EXECUTE;
        }
        if a.contains(Arm64Attr::AP_EL0) && !a.contains(Arm64Attr::AP_NO_EL0_TABLE) {
            f |= Self::USER;
        }
        if let Some(attr) = a.mem_attr() {
            match attr {
                Arm64MemAttr::Device => f |= Self::DEVICE | Self::UNCACHED,
                Arm64MemAttr::NormalNonCacheable => f |= Self::UNCACHED,
                _ => {}
            }
        }
        f
    }
}

impl From<PagingFlags> for Arm64Attr {
    fn from(f: PagingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut a = Self::VALID | Self::AF | Self::NON_BLOCK;
        if !f.contains(PagingFlags::WRITE) {
            a |= Self::AP_RO;
        }
        if !f.contains(PagingFlags::EXECUTE) {
            a |= Self::UXN | Self::PXN;
        }
        if f.contains(PagingFlags::USER) {
            a |= Self::AP_EL0;
        }
        if f.contains(PagingFlags::SHARED) {
            a |= Self::NS;
        }
        let mem_attr = if f.contains(PagingFlags::DEVICE) {
            Arm64MemAttr::Device
        } else if f.contains(PagingFlags::UNCACHED) {
            Arm64MemAttr::NormalNonCacheable
        } else {
            Arm64MemAttr::Normal
        };
        a | Self::from_mem_attr(mem_attr)
    }
}

/// AArch64 page table entry (PTE).
///
/// A `#[repr(transparent)]` wrapper around `u64` that encodes physical
/// addresses and [`Arm64Attr`] attributes. Physical addresses are masked
/// to cover bits 12–47 (48-bit physical address space).
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct A64PageEntry(u64);

impl A64PageEntry {
    const PADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
}

impl PageTableEntry for A64PageEntry {
    crate::impl_pte_common_ops!(Arm64Attr, Self::PADDR_MASK);

    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut a = Arm64Attr::from(flags);
        if is_huge {
            a.remove(Arm64Attr::NON_BLOCK);
        }
        Self(a.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        let a = Arm64Attr::VALID
            | Arm64Attr::NON_BLOCK
            | Arm64Attr::from_mem_attr(Arm64MemAttr::Normal);
        Self(a.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        let mut a = Arm64Attr::from(flags);
        if is_huge {
            a.remove(Arm64Attr::NON_BLOCK);
        }
        self.0 = (self.0 & Self::PADDR_MASK) | a.bits();
    }

    fn is_present(&self) -> bool {
        Arm64Attr::from_bits_truncate(self.0).contains(Arm64Attr::VALID)
    }

    fn is_huge(&self) -> bool {
        let a = Arm64Attr::from_bits_truncate(self.0);
        a.contains(Arm64Attr::VALID) && !a.contains(Arm64Attr::NON_BLOCK)
    }
}

crate::impl_pte_debug!(A64PageEntry);

/// AArch64 paging metadata: 4-level paging, 48-bit PA, 48-bit VA.
///
/// AArch64 uses Inner Shareable TLBI instructions for SMP TLB flush,
/// so `flush_tlb_all_cpus` does not need software IPI shootdown.
pub struct A64PagingMetaData;

impl PagingMetaData for A64PagingMetaData {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 48;
    const VA_MAX_BITS: usize = 48;

    fn vaddr_is_valid(vaddr: usize) -> bool {
        let top_bits = vaddr >> Self::VA_MAX_BITS;
        top_bits == 0 || top_bits == 0xffff
    }

    #[inline]
    fn flush_tlb(vaddr: Option<VirtAddr>) {
        karch::flush_tlb(vaddr);
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_process(vaddr: Option<VirtAddr>) {
        // AArch64 Inner Shareable TLBI instructions (vaae1is / vmalle1is)
        // broadcast to all PEs in the same Inner Shareable domain, so no
        // software IPI shootdown is needed.
        karch::flush_tlb(vaddr);
    }

    #[inline]
    fn flush_tlb_process_asid(vaddr: Option<VirtAddr>, asid: u16) {
        match vaddr {
            Some(va) => karch::flush_tlb_va_asid(va, asid),
            None => karch::flush_tlb_asid(asid),
        }
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<VirtAddr>) {
        // Same as flush_tlb_process — hardware broadcast already covers
        // all inner-shareable CPUs.  Provided for clarity: kernel page
        // table flushes have the same implementation as user page table
        // flushes on aarch64.
        karch::flush_tlb(vaddr);
    }
}

/// AArch64 page table type alias.
pub type A64PageTable<H> = PageTable64<A64PagingMetaData, A64PageEntry, H>;
/// AArch64 mutable page table type alias.
pub type A64PageTableMut<'a, H> = PageTableMut<'a, A64PagingMetaData, A64PageEntry, H>;
