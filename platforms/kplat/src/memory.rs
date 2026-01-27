use core::{
    fmt,
    ops::{Deref, DerefMut, Range},
};

use kplat_macros::device_interface;
pub use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, pa, va};

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    pub struct MemFlags: usize {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
        const DEVICE = 1 << 4;
        const UNCACHED = 1 << 5;
        const RESERVED = 1 << 6;
        const FREE = 1 << 7;

        const R = 1 << 0;
        const W = 1 << 1;
        const X = 1 << 2;
        const DEV = 1 << 4;
        const UC = 1 << 5;
        const RSVD = 1 << 6;
    }
}

impl fmt::Debug for MemFlags {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

pub const RAM_DEF: MemFlags = MemFlags::R.union(MemFlags::W).union(MemFlags::FREE);
pub const RSVD_DEF: MemFlags = MemFlags::R.union(MemFlags::W).union(MemFlags::RSVD);
pub const MMIO_DEF: MemFlags = MemFlags::R
    .union(MemFlags::W)
    .union(MemFlags::DEV)
    .union(MemFlags::RSVD);

pub type MemRange = (usize, usize);

#[repr(align(4096))]
pub struct PageAligned<T: Sized>(T);

impl<T: Sized> PageAligned<T> {
    pub const fn new(v: T) -> Self {
        Self(v)
    }
}

impl<T> Deref for PageAligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for PageAligned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub paddr: PhysAddr,
    pub size: usize,
    pub flags: MemFlags,
    pub name: &'static str,
}

impl MemoryRegion {
    pub const fn new_ram(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            size: n,
            flags: RAM_DEF,
            name,
        }
    }

    pub const fn new_mmio(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            size: n,
            flags: MMIO_DEF,
            name,
        }
    }

    pub const fn new_rsvd(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            size: n,
            flags: RSVD_DEF,
            name,
        }
    }
}

#[device_interface]
pub trait HwMemory {
    fn ram_regions() -> &'static [MemRange];
    fn rsvd_regions() -> &'static [MemRange];
    fn mmio_regions() -> &'static [MemRange];
    fn p2v(pa: PhysAddr) -> VirtAddr;
    fn v2p(va: VirtAddr) -> PhysAddr;
    fn kernel_layout() -> (VirtAddr, usize);
}

pub fn total_ram() -> usize {
    ram_regions().iter().map(|r| r.1).sum()
}

pub type OverlapError = (Range<usize>, Range<usize>);

pub fn check_overlap(iter: impl Iterator<Item = MemRange>) -> Result<(), OverlapError> {
    let mut last = Range::default();
    for (s, n) in iter {
        if last.end > s {
            return Err((last, s..s + n));
        }
        last = s..s + n;
    }
    Ok(())
}

pub fn sub_ranges<F>(base: &[MemRange], cut: &[MemRange], mut cb: F) -> Result<(), OverlapError>
where
    F: FnMut(MemRange),
{
    check_overlap(cut.iter().cloned())?;

    for &(mut s, n) in base {
        let e = s + n;

        for &(cs, cn) in cut {
            let ce = cs + cn;
            if ce <= s {
                continue;
            }
            if cs >= e {
                break;
            }
            if cs > s {
                cb((s, cs - s));
            }
            s = ce;
        }
        if s < e {
            cb((s, e - s));
        }
    }
    Ok(())
}
