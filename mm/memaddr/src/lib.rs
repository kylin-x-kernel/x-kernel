//! Memory address types, ranges, and alignment utilities.
#![cfg_attr(not(test), no_std)]

mod units;

pub use self::units::{
    AddrOps, AddrRange, DynPageIter, MemoryAddr, PageIter, PhysAddr, PhysAddrRange, VirtAddr,
    VirtAddrRange,
};

/// 4 KiB page size.
pub const PAGE_SIZE_4K: usize = 0x1000;
/// 2 MiB page size.
pub const PAGE_SIZE_2M: usize = 0x20_0000;
/// 1 GiB page size.
pub const PAGE_SIZE_1G: usize = 0x4000_0000;

pub type PageIter4K<A> = PageIter<PAGE_SIZE_4K, A>;
pub type PageIter2M<A> = PageIter<PAGE_SIZE_2M, A>;
pub type PageIter1G<A> = PageIter<PAGE_SIZE_1G, A>;

/// Align down to the nearest multiple of `align`.
pub const fn floor_align(addr: usize, align: usize) -> usize {
    let mask = align - 1;
    addr & !mask
}

/// Align up to the nearest multiple of `align`.
pub const fn ceil_align(addr: usize, align: usize) -> usize {
    let mask = align - 1;
    (addr + mask) & !mask
}

/// Return the remainder for `addr` relative to `align`.
pub const fn align_rem(addr: usize, align: usize) -> usize {
    addr & (align - 1)
}

/// Returns `true` if `addr` is aligned to `align`.
pub const fn aligned_to(addr: usize, align: usize) -> bool {
    align_rem(addr, align) == 0
}

/// Align down to a 4 KiB boundary.
pub const fn floor_4k(addr: usize) -> usize {
    floor_align(addr, PAGE_SIZE_4K)
}

/// Align up to a 4 KiB boundary.
pub const fn ceil_4k(addr: usize) -> usize {
    ceil_align(addr, PAGE_SIZE_4K)
}

/// Return the 4 KiB alignment remainder.
pub const fn rem_4k(addr: usize) -> usize {
    align_rem(addr, PAGE_SIZE_4K)
}

/// Returns `true` if `addr` is 4 KiB aligned.
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
