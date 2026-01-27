use core::fmt;

use memaddr::{MemoryAddr, PhysAddr, VirtAddr};

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq)]
    pub struct PagingFlags: usize {
        const READ          = 1 << 0;
        const WRITE         = 1 << 1;
        const EXECUTE       = 1 << 2;
        const USER          = 1 << 3;
        const DEVICE        = 1 << 4;
        const UNCACHED      = 1 << 5;
    }
}

impl fmt::Debug for PagingFlags {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

pub trait PageTableEntry: fmt::Debug + Clone + Copy + Sync + Send + Sized {
    fn new_page(paddr: PhysAddr, flags: PagingFlags, is_huge: bool) -> Self;
    fn new_table(paddr: PhysAddr) -> Self;
    fn paddr(&self) -> PhysAddr;
    fn flags(&self) -> PagingFlags;
    fn set_paddr(&mut self, paddr: PhysAddr);
    fn set_flags(&mut self, flags: PagingFlags, is_huge: bool);
    fn bits(self) -> usize;
    fn is_unused(&self) -> bool;
    fn is_present(&self) -> bool;
    fn is_huge(&self) -> bool;
    fn clear(&mut self);
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PtError {
    NoMemory,
    NotAligned,
    NotMapped,
    AlreadyMapped,
    MappedToHugePage,
}

#[cfg(feature = "axerrno")]
impl From<PtError> for axerrno::AxError {
    fn from(value: PtError) -> Self {
        match value {
            PtError::NoMemory => axerrno::AxError::NoMemory,
            _ => axerrno::AxError::InvalidInput,
        }
    }
}

pub type PtResult<T = ()> = Result<T, PtError>;

pub trait PagingMetaData: Sync + Send {
    const LEVELS: usize;
    const PA_MAX_BITS: usize;
    const VA_MAX_BITS: usize;
    const PA_MAX_ADDR: usize = (1 << Self::PA_MAX_BITS) - 1;
    type VirtAddr: MemoryAddr;

    fn paddr_is_valid(paddr: usize) -> bool {
        paddr <= Self::PA_MAX_ADDR
    }

    fn vaddr_is_valid(vaddr: usize) -> bool {
        let top_mask = usize::MAX << (Self::VA_MAX_BITS - 1);
        (vaddr & top_mask) == 0 || (vaddr & top_mask) == top_mask
    }

    fn flush_tlb(vaddr: Option<Self::VirtAddr>);
}

pub trait PagingHandler: Sized {
    fn alloc_frame() -> Option<PhysAddr>;
    fn dealloc_frame(paddr: PhysAddr);
    fn p2v(paddr: PhysAddr) -> VirtAddr;
}

#[repr(usize)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PageSize {
    Size4K = 0x1000,
    Size2M = 0x20_0000,
    Size1G = 0x4000_0000,
}

impl PageSize {
    pub const fn is_huge(self) -> bool {
        matches!(self, Self::Size1G | Self::Size2M)
    }

    pub const fn is_aligned(self, addr_or_size: usize) -> bool {
        memaddr::is_aligned(addr_or_size, self as usize)
    }

    pub const fn align_offset(self, addr: usize) -> usize {
        memaddr::align_offset(addr, self as usize)
    }
}

impl From<PageSize> for usize {
    fn from(size: PageSize) -> usize {
        size as usize
    }
}
