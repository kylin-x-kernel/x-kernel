use core::{arch::asm, fmt};

use memaddr::{PhysAddr, VirtAddr};

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct LaFlags: u64 {
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

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct La64PageEntry(u64);

impl La64PageEntry {
    const PADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

    pub const fn empty() -> Self {
        Self(0)
    }
}

impl PageTableEntry for La64PageEntry {
    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut f = LaFlags::from(flags);
        if is_huge {
            f |= LaFlags::GH;
        }
        Self(f.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        Self(LaFlags::V.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from((self.0 & Self::PADDR_MASK) as usize)
    }

    fn flags(&self) -> PagingFlags {
        LaFlags::from_bits_truncate(self.0).into()
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !Self::PADDR_MASK) | (paddr.as_usize() as u64 & Self::PADDR_MASK);
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        let mut f = LaFlags::from(flags);
        if is_huge {
            f |= LaFlags::GH;
        }
        self.0 = (self.0 & Self::PADDR_MASK) | f.bits();
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_unused(&self) -> bool {
        self.0 == 0
    }

    fn is_present(&self) -> bool {
        LaFlags::from_bits_truncate(self.0).contains(LaFlags::V)
    }

    fn is_huge(&self) -> bool {
        LaFlags::from_bits_truncate(self.0).contains(LaFlags::GH)
    }

    fn clear(&mut self) {
        self.0 = 0;
    }
}

impl fmt::Debug for La64PageEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("La64PageEntry")
            .field("paddr", &self.paddr())
            .field("flags", &self.flags())
            .finish()
    }
}

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
        if let Some(vaddr) = vaddr {
            unsafe {
                asm!("invtlb 0x01, $r0, {}", in(reg) vaddr.as_usize());
            }
        } else {
            unsafe {
                asm!("invtlb 0x00, $r0, $r0");
            }
        }
    }
}

pub type LA64PageTable<H> = PageTable64<LA64MetaData, La64PageEntry, H>;
pub type LA64PageTableMut<'a, H> = PageTableMut<'a, LA64MetaData, La64PageEntry, H>;
