// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical memory management.

use heapless::Vec;
pub use kplat::memory::{
    MemFlags, MemoryRegion, kernel_layout, mmio_regions, p2v, ram_regions, rsvd_regions, total_ram,
    v2p,
};
use kplat::memory::{check_overlap, sub_ranges};
use lazyinit::LazyInit;
pub use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange, pa, va};

use crate::addr_of_sym;

const MAX_REGIONS: usize = 128;

static ALL_MEM_REGIONS: LazyInit<Vec<MemoryRegion, MAX_REGIONS>> = LazyInit::new();

/// Returns an iterator over all physical memory regions.
pub fn memory_regions() -> impl Iterator<Item = MemoryRegion> {
    ALL_MEM_REGIONS.iter().cloned()
}

/// Fills the `.bss` section with zeros.
///
/// It requires the symbols `_sbss` and `_ebss` to be defined in the linker script.
///
/// # Safety
///
/// This function is unsafe because it writes `.bss` section directly.
pub unsafe fn clear_bss() {
    unsafe {
        core::slice::from_raw_parts_mut(
            _sbss as *mut u8,
            (_ebss as *mut u8).offset_from_unsigned(_sbss as *mut u8),
        )
        .fill(0);
    }
}

/// Initializes physical memory regions.
pub fn init() {
    let mut all_regions = Vec::new();
    let mut push = |r: MemoryRegion| {
        if r.size > 0 {
            all_regions.push(r).expect("too many memory regions");
        }
    };

    // Push regions in kernel image
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_stext).into()),
        size: addr_of_sym!(_etext) - addr_of_sym!(_stext),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::X,
        name: ".text",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_srodata).into()),
        size: addr_of_sym!(_erodata) - addr_of_sym!(_srodata),
        flags: MemFlags::RSVD | MemFlags::R,
        name: ".rodata",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_sdata).into()),
        size: addr_of_sym!(_edata) - addr_of_sym!(_sdata),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: ".data .tdata .tbss .percpu",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(boot_stack).into()),
        size: addr_of_sym!(boot_stack_top) - addr_of_sym!(boot_stack),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: "boot stack",
    });
    push(MemoryRegion {
        paddr: v2p(addr_of_sym!(_sbss).into()),
        size: addr_of_sym!(_ebss) - addr_of_sym!(_sbss),
        flags: MemFlags::RSVD | MemFlags::R | MemFlags::W,
        name: ".bss",
    });

    // Push MMIO & reserved regions
    for &(start, size) in mmio_regions() {
        push(MemoryRegion::new_mmio(start, size, "mmio"));
    }
    for &(start, size) in rsvd_regions() {
        push(MemoryRegion::new_rsvd(start, size, "reserved"));
    }

    // Combine kernel image range and reserved ranges
    let kernel_start = v2p(addr_of_sym!(_skernel).into()).as_usize();
    let kernel_size = addr_of_sym!(_ekernel) - addr_of_sym!(_skernel);
    let mut reserved_ranges = rsvd_regions()
        .iter()
        .cloned()
        .chain(core::iter::once((kernel_start, kernel_size))) // kernel image range is also reserved
        .collect::<Vec<_, MAX_REGIONS>>();

    // Remove all reserved ranges from RAM ranges, and push the remaining as free memory
    reserved_ranges.sort_unstable_by_key(|&(start, _size)| start);
    sub_ranges(ram_regions(), &reserved_ranges, |(start, size)| {
        push(MemoryRegion::new_ram(start, size, "free memory"));
    })
    .inspect_err(|(a, b)| error!("Reserved memory region {a:#x?} overlaps with {b:#x?}"))
    .unwrap();

    // Check overlapping
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
