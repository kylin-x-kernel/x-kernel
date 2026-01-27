use core::fmt;

use memaddr::{PhysAddr, VirtAddr};
pub use x86_64::structures::paging::page_table::PageTableFlags as PTF;

use crate::{
    defs::{PageTableEntry, PagingFlags, PagingMetaData},
    table64::{PageTable64, PageTableMut},
};

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

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X64PageEntry(u64);

impl X64PageEntry {
    const PADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    pub const fn empty() -> Self {
        Self(0)
    }
}

impl PageTableEntry for X64PageEntry {
    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self {
        let mut f = PTF::from(flags);
        if is_huge {
            f |= PTF::HUGE_PAGE;
        }
        Self(f.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn new_table(paddr: PhysAddr) -> Self {
        let f = PTF::PRESENT | PTF::WRITABLE | PTF::USER_ACCESSIBLE;
        Self(f.bits() | (paddr.as_usize() as u64 & Self::PADDR_MASK))
    }

    fn paddr(&self) -> PhysAddr {
        PhysAddr::from((self.0 & Self::PADDR_MASK) as usize)
    }

    fn flags(&self) -> PagingFlags {
        PTF::from_bits_truncate(self.0).into()
    }

    fn set_paddr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !Self::PADDR_MASK) | (paddr.as_usize() as u64 & Self::PADDR_MASK)
    }

    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool) {
        let mut f = PTF::from(flags);
        if is_huge {
            f |= PTF::HUGE_PAGE;
        }
        self.0 = (self.0 & Self::PADDR_MASK) | f.bits()
    }

    fn bits(self) -> usize {
        self.0 as usize
    }

    fn is_unused(&self) -> bool {
        self.0 == 0
    }

    fn is_present(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::PRESENT)
    }

    fn is_huge(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::HUGE_PAGE)
    }

    fn clear(&mut self) {
        self.0 = 0;
    }
}

impl fmt::Debug for X64PageEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut f = f.debug_struct("X64PageEntry");
        f.field("paddr", &self.paddr());
        f.field("flags", &self.flags());
        f.finish()
    }
}

pub struct X64PagingMetaData;

impl PagingMetaData for X64PagingMetaData {
    type VirtAddr = VirtAddr;

    const LEVELS: usize = 4;
    const PA_MAX_BITS: usize = 52;
    const VA_MAX_BITS: usize = 48;

    #[inline]
    fn flush_tlb(vaddr: Option<VirtAddr>) {
        if let Some(vaddr) = vaddr {
            x86_64::instructions::tlb::flush(x86_64::VirtAddr::new(vaddr.as_usize() as u64));
        } else {
            x86_64::instructions::tlb::flush_all();
        }
    }
}

pub type X64PageTable<H> = PageTable64<X64PagingMetaData, X64PageEntry, H>;
pub type X64PageTableMut<'a, H> = PageTableMut<'a, X64PagingMetaData, X64PageEntry, H>;
