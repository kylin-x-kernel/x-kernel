// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::{PhysAddr, VirtAddr};

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct RvFlags: usize {
        const V =   1 << 0;
        const R =   1 << 1;
        const W =   1 << 2;
        const X =   1 << 3;
        const U =   1 << 4;
        const G =   1 << 5;
        const A =   1 << 6;
        const D =   1 << 7;
    }
}

impl From<RvFlags> for PagingFlags {
    fn from(f: RvFlags) -> Self {
        let mut ret = Self::empty();
        if !f.contains(RvFlags::V) {
            return ret;
        }
        if f.contains(RvFlags::R) {
            ret |= Self::READ;
        }
        if f.contains(RvFlags::W) {
            ret |= Self::WRITE;
        }
        if f.contains(RvFlags::X) {
            ret |= Self::EXECUTE;
        }
        if f.contains(RvFlags::U) {
            ret |= Self::USER;
        }
        ret
    }
}

impl From<PagingFlags> for RvFlags {
    fn from(f: PagingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::V;
        if f.contains(PagingFlags::READ) {
            ret |= Self::R;
        }
        if f.contains(PagingFlags::WRITE) {
            ret |= Self::W;
        }
        if f.contains(PagingFlags::EXECUTE) {
            ret |= Self::X;
        }
        if f.contains(PagingFlags::USER) {
            ret |= Self::U;
        }
        ret
    }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Rv64PageEntry(u64);

impl Rv64PageEntry {
    const PADDR_MASK: u64 = (1 << 54) - (1 << 10);
}

impl PageTableEntry for Rv64PageEntry {
    const EMPTY: Self = Self(0);

    fn new_page(paddr: PhysAddr, flags: PagingFlags, _is_huge: bool) -> Self {
        let f = RvFlags::from(flags) | RvFlags::A | RvFlags::D;
        Self(f.bits() as u64 | ((paddr.as_usize() >> 2) as u64 & Self::PADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        Self(RvFlags::V.bits() as u64 | ((paddr.as_usize() >> 2) as u64 & Self::PADDR_MASK))
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from(((self.0 & Self::PADDR_MASK) << 2) as usize)
    }

    fn flags(&self) -> PagingFlags {
        RvFlags::from_bits_truncate(self.0 as usize).into()
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !Self::PADDR_MASK) | ((paddr.as_usize() >> 2) as u64 & Self::PADDR_MASK);
    }

    fn set_flags(&mut self, flags: PagingFlags, _is_huge: bool) {
        let f = RvFlags::from(flags) | RvFlags::A | RvFlags::D;
        self.0 = (self.0 & Self::PADDR_MASK) | f.bits() as u64;
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_present(&self) -> bool {
        RvFlags::from_bits_truncate(self.0 as usize).contains(RvFlags::V)
    }

    fn is_huge(&self) -> bool {
        let f = RvFlags::from_bits_truncate(self.0 as usize);
        f.contains(RvFlags::V) && (f.contains(RvFlags::R) || f.contains(RvFlags::X))
    }
}

crate::impl_pte_debug!(Rv64PageEntry);

pub trait SvVirtAddr: memaddr::MemoryAddr + Send + Sync {
    fn flush_tlb(vaddr: Option<Self>);

    #[inline]
    fn flush_tlb_process(vaddr: Option<Self>) {
        Self::flush_tlb(vaddr);
    }

    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<Self>) {
        Self::flush_tlb_process(vaddr);
    }
}

impl SvVirtAddr for VirtAddr {
    #[inline]
    fn flush_tlb(vaddr: Option<Self>) {
        karch::flush_tlb(vaddr);
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_process(vaddr: Option<Self>) {
        karch::flush_tlb(vaddr);
        crate_interface::call_interface!(crate::defs::TlbFlushIf::flush_process(vaddr));
    }

    #[cfg(feature = "smp")]
    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<Self>) {
        karch::flush_tlb(vaddr);
        crate_interface::call_interface!(crate::defs::TlbFlushIf::flush_all_cpus(vaddr));
    }
}

pub struct Sv39MetaData<VA: SvVirtAddr> {
    _virt_addr: core::marker::PhantomData<VA>,
}

pub struct Sv48MetaData<VA: SvVirtAddr> {
    _virt_addr: core::marker::PhantomData<VA>,
}

impl<VA: SvVirtAddr> PagingMetaData for Sv39MetaData<VA> {
    type VirtAddr = VA;

    const LEVELS: usize = 3;
    const PA_MAX_BITS: usize = 56;
    const VA_MAX_BITS: usize = 39;

    #[inline]
    fn flush_tlb(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb(vaddr);
    }

    #[inline]
    fn flush_tlb_process(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb_process(vaddr);
    }

    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb_all_cpus(vaddr);
    }
}

impl<VA: SvVirtAddr> PagingMetaData for Sv48MetaData<VA> {
    type VirtAddr = VA;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 56;
    const VA_MAX_BITS: usize = 48;

    #[inline]
    fn flush_tlb(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb(vaddr);
    }

    #[inline]
    fn flush_tlb_process(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb_process(vaddr);
    }

    #[inline]
    fn flush_tlb_all_cpus(vaddr: Option<VA>) {
        <VA as SvVirtAddr>::flush_tlb_all_cpus(vaddr);
    }
}

pub type Sv39PageTable<H> = PageTable64<Sv39MetaData<VirtAddr>, Rv64PageEntry, H>;
pub type Sv39PageTableMut<'a, H> = PageTableMut<'a, Sv39MetaData<VirtAddr>, Rv64PageEntry, H>;

pub type Sv48PageTable<H> = PageTable64<Sv48MetaData<VirtAddr>, Rv64PageEntry, H>;
pub type Sv48PageTableMut<'a, H> = PageTableMut<'a, Sv48MetaData<VirtAddr>, Rv64PageEntry, H>;
