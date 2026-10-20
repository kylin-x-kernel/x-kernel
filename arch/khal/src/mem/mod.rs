// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical memory management.

mod desc;
pub mod reserved;
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
///
/// Call exactly once during primary boot, before
/// `memspace::init_memory_management` replaces the boot page table: the
/// platform hook and the firmware memory description read boot-protocol
/// tables (for example the x86 Linux zeropage) through their physical
/// addresses, which the early identity mapping makes readable.
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
        name: ".data",
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
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: reserved_regions sorts and rejects \
                  overlapping reservations before this subtraction"
    )]
    sub_ranges(ram_regions, &reserved_ranges, |(start, size)| {
        push(MemoryRegion::new_ram(start, size, "free memory"));
    })
    .inspect_err(|(a, b)| error!("Reserved memory region {a:#x?} overlaps with {b:#x?}"))
    .unwrap();

    all_regions.sort_unstable_by_key(|r| r.paddr);
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: overlapping final physical memory regions \
                  make allocator startup unsafe; this boot entry has no error return or caller \
                  that can recover"
    )]
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
unittest::path_mod_test!(pub mod tests_mem, "../../ut-tests/mem.rs");
