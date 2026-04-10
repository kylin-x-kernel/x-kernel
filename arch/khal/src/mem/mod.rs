// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical memory management.

mod desc;
mod reserved;
mod source;

use heapless::Vec;
use lazyinit::LazyInit;
pub use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange, pa, va};

pub(crate) use self::source::{
    check_overlap, described_ram_regions, described_reserved_regions, has_described_regions,
    init_described_regions,
};
pub use self::{
    desc::{
        Aligned4K, MemFlags, MemRange, MemoryRegion, PageAligned, RawRange, ReservedKind,
        ReservedRegion, ReservedSource,
    },
    source::{sub_ranges, total_ram},
};
use crate::addr_of_sym;

const MAX_REGIONS: usize = 256;

static ALL_MEM_REGIONS: LazyInit<Vec<MemoryRegion, MAX_REGIONS>> = LazyInit::new();

#[inline]
pub fn p2v(paddr: PhysAddr) -> VirtAddr {
    VirtAddr::from_usize(kaddr_layout::p2v(paddr.as_usize()))
}

#[inline]
pub fn v2p(vaddr: VirtAddr) -> PhysAddr {
    PhysAddr::from_usize(kaddr_layout::v2p(vaddr.as_usize()))
}

/// Returns an iterator over all physical memory regions.
pub fn memory_regions() -> impl Iterator<Item = MemoryRegion> {
    ALL_MEM_REGIONS.iter().cloned()
}

/// Initializes physical memory regions.
pub fn init(boot_info: &boot_info::BootInfo) {
    kplat::boot::prepare_boot_memory(boot_info);
    if !has_described_regions() {
        crate::firmware::init_memory_description(boot_info);
    }

    let mut all_regions = Vec::new();
    let mut push = |r: MemoryRegion| {
        if r.size > 0 {
            all_regions.push(r).expect("too many memory regions");
        }
    };

    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_stext).into()),
        vaddr: Some(va!(addr_of_sym!(_stext))),
        size: addr_of_sym!(_etext) - addr_of_sym!(_stext),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::X,
        name: ".text",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_srodata).into()),
        vaddr: Some(va!(addr_of_sym!(_srodata))),
        size: addr_of_sym!(_erodata) - addr_of_sym!(_srodata),
        flags: MemFlags::RSVD | MemFlags::R,
        name: ".rodata",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_sdata).into()),
        vaddr: Some(va!(addr_of_sym!(_sdata))),
        size: addr_of_sym!(_edata) - addr_of_sym!(_sdata),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: ".data .tdata .tbss .percpu",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(boot_stack).into()),
        vaddr: Some(va!(addr_of_sym!(boot_stack))),
        size: addr_of_sym!(boot_stack_top) - addr_of_sym!(boot_stack),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: "boot stack",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_sbss).into()),
        vaddr: Some(va!(addr_of_sym!(_sbss))),
        size: addr_of_sym!(_ebss) - addr_of_sym!(_sbss),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: ".bss",
    });

    let kernel_start = v2p(addr_of_sym!(_skernel).into()).as_usize();
    let kernel_end = v2p(addr_of_sym!(_ekernel).into()).as_usize();
    let reserved = reserved::reserved_regions(kernel_start, kernel_end - kernel_start);
    let reserved_ranges = reserved::append_reserved_memory_regions(&reserved, &mut push);
    let ram_regions = described_ram_regions();
    sub_ranges(ram_regions, &reserved_ranges, |(start, size)| {
        push(MemoryRegion::new_ram(start, size, "free memory"));
    })
    .inspect_err(|(a, b)| error!("Reserved memory region {a:#x?} overlaps with {b:#x?}"))
    .unwrap();

    all_regions.sort_unstable_by_key(|r| r.paddr);
    check_overlap(all_regions.iter().map(|r| (r.paddr.into(), r.size)))
        .inspect_err(|(a, b)| error!("Physical memory region {a:#x?} overlaps with {b:#x?}"))
        .unwrap();

    ALL_MEM_REGIONS.init_once(all_regions);
}

unsafe extern "C" {
    fn _stext();
    fn _etext();
    fn _srodata();
    fn _erodata();
    fn _sdata();
    fn _edata();
    fn _sbss();
    fn _ebss();
    fn _skernel();
    fn _ekernel();
    fn boot_stack();
    fn boot_stack_top();
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_mem {
    use unittest::def_test;

    use super::{MemFlags, MemoryRegion};

    #[def_test]
    fn test_memory_region_new_ram() {
        let region = MemoryRegion::new_ram(0x1000, 0x2000, "ram");
        assert_eq!(region.paddr.as_usize(), 0x1000);
        assert_eq!(region.size, 0x2000);
        assert!(region.flags.contains(MemFlags::R));
        assert!(region.flags.contains(MemFlags::W));
        assert!(region.flags.contains(MemFlags::FREE));
    }

    #[def_test]
    fn test_memory_region_new_mmio() {
        let region = MemoryRegion::new_mmio(0x3000, 0x1000, "mmio");
        assert_eq!(region.size, 0x1000);
        assert!(region.flags.contains(MemFlags::DEV));
        assert!(region.flags.contains(MemFlags::RSVD));
    }

    #[def_test]
    fn test_memory_region_new_rsvd() {
        let region = MemoryRegion::new_rsvd(0x4000, 0x800, "rsvd");
        assert_eq!(region.size, 0x800);
        assert!(region.flags.contains(MemFlags::RSVD));
        assert!(!region.flags.contains(MemFlags::FREE));
    }
}
