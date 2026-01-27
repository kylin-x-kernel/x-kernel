//! x86 page table entries on 64-bit paging.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use memory_addr::PhysAddr;

pub use x86_64::structures::paging::page_table::PageTableFlags as PTF;

use crate::{GenericPTE, MappingFlags};

/// Global C-Bit mask for AMD SEV.
/// This is initialized once and then used for all page table operations.
static SEV_CBIT_MASK: AtomicU64 = AtomicU64::new(0);

/// Initialize the SEV C-Bit mask.
/// This should be called early during boot on AMD SEV platforms.
#[inline(never)]
pub fn init_sev_cbit(cbit_position: u8) {
    if cbit_position > 0 && cbit_position < 64 {
        SEV_CBIT_MASK.store(1u64 << cbit_position, Ordering::SeqCst);
    }
}

/// Get the current SEV C-Bit mask.
#[inline]
fn get_sev_cbit_mask() -> u64 {
    SEV_CBIT_MASK.load(Ordering::SeqCst)
}

impl From<PTF> for MappingFlags {
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

impl From<MappingFlags> for PTF {
    fn from(f: MappingFlags) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::PRESENT;
        if f.contains(MappingFlags::WRITE) {
            ret |= Self::WRITABLE;
        }
        if !f.contains(MappingFlags::EXECUTE) {
            ret |= Self::NO_EXECUTE;
        }
        if f.contains(MappingFlags::USER) {
            ret |= Self::USER_ACCESSIBLE;
        }
        if f.contains(MappingFlags::DEVICE) || f.contains(MappingFlags::UNCACHED) {
            ret |= Self::NO_CACHE | Self::WRITE_THROUGH;
        }
        ret
    }
}

/// An x86_64 page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X64PTE(u64);

impl X64PTE {
    const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000; // bits 12..52

    /// Creates an empty descriptor with all bits set to zero.
    pub const fn empty() -> Self {
        Self(0)
    }
}

impl GenericPTE for X64PTE {
    fn new_page(paddr: PhysAddr, flags: MappingFlags, is_huge: bool) -> Self {
        let mut ptf = PTF::from(flags);
        if is_huge {
            ptf |= PTF::HUGE_PAGE;
        }
        // Add C-Bit for AMD SEV encrypted pages, but NOT for shared memory
        let cbit = if flags.contains(MappingFlags::SHARED) {
            0
        } else {
            get_sev_cbit_mask()
        };
        let paddr_with_cbit = paddr.as_usize() as u64 | cbit;
        Self(ptf.bits() | (paddr_with_cbit & Self::PHYS_ADDR_MASK))
    }
    fn new_table(paddr: PhysAddr) -> Self {
        let flags = PTF::PRESENT | PTF::WRITABLE | PTF::USER_ACCESSIBLE;
        // Page table pages are always encrypted (with C-Bit)
        let paddr_with_cbit = paddr.as_usize() as u64 | get_sev_cbit_mask();
        Self(flags.bits() | (paddr_with_cbit & Self::PHYS_ADDR_MASK))
    }
    fn paddr(&self) -> PhysAddr {
        // Remove C-Bit when returning physical address
        let paddr = (self.0 & Self::PHYS_ADDR_MASK) & !get_sev_cbit_mask();
        PhysAddr::from(paddr as usize)
    }
    fn flags(&self) -> MappingFlags {
        PTF::from_bits_truncate(self.0).into()
    }
    fn set_paddr(&mut self, paddr: PhysAddr) {
        // Add C-Bit for AMD SEV encrypted pages
        let paddr_with_cbit = paddr.as_usize() as u64 | get_sev_cbit_mask();
        self.0 = (self.0 & !Self::PHYS_ADDR_MASK) | (paddr_with_cbit & Self::PHYS_ADDR_MASK)
    }
    fn set_flags(&mut self, flags: MappingFlags, is_huge: bool) {
        let mut flags = PTF::from(flags);
        if is_huge {
            flags |= PTF::HUGE_PAGE;
        }
        self.0 = (self.0 & Self::PHYS_ADDR_MASK) | flags.bits()
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
        self.0 = 0
    }
}

impl fmt::Debug for X64PTE {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut f = f.debug_struct("X64PTE");
        f.field("raw", &self.0)
            .field("paddr", &self.paddr())
            .field("flags", &self.flags())
            .finish()
    }
}
