#![cfg_attr(not(test), no_std)]

mod units;

pub use self::units::{AddrOps, AddrRange, DynPageIter, MemoryAddr, PageIter, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

pub const PAGE_SIZE_4K: usize = 0x1000;
pub const PAGE_SIZE_2M: usize = 0x20_0000;
pub const PAGE_SIZE_1G: usize = 0x4000_0000;

pub type PageIter4K<A> = PageIter<PAGE_SIZE_4K, A>;
pub type PageIter2M<A> = PageIter<PAGE_SIZE_2M, A>;
pub type PageIter1G<A> = PageIter<PAGE_SIZE_1G, A>;

pub const fn floor_align(addr: usize, align: usize) -> usize {
    let mask = align - 1;
    addr & !mask
}

pub const fn ceil_align(addr: usize, align: usize) -> usize {
    let mask = align - 1;
    (addr + mask) & !mask
}

pub const fn align_rem(addr: usize, align: usize) -> usize {
    addr & (align - 1)
}

pub const fn aligned_to(addr: usize, align: usize) -> bool {
    align_rem(addr, align) == 0
}

pub const fn floor_4k(addr: usize) -> usize {
    floor_align(addr, PAGE_SIZE_4K)
}

pub const fn ceil_4k(addr: usize) -> usize {
    ceil_align(addr, PAGE_SIZE_4K)
}

pub const fn rem_4k(addr: usize) -> usize {
    align_rem(addr, PAGE_SIZE_4K)
}

pub const fn aligned_4k(addr: usize) -> bool {
    aligned_to(addr, PAGE_SIZE_4K)
}

pub use align_rem as align_offset;
pub use aligned_4k as is_aligned_4k;
pub use aligned_to as is_aligned;
pub use ceil_4k as align_up_4k;
pub use ceil_align as align_up;
pub use floor_4k as align_down_4k;
pub use floor_align as align_down;
pub use rem_4k as align_offset_4k;
