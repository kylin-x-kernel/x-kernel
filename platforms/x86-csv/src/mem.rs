// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use heapless::Vec;
use kbuild_config::{MMIO_RANGES, PHYS_MEM_BASE, PHYS_MEM_SIZE};
use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, default_p2v, default_v2p, va};
use lazyinit::LazyInit;
use x86_peripherals::bootmem::for_each_ram_region;

const MAX_REGIONS: usize = 16;
static RAM_REGIONS: LazyInit<Vec<MemRange, MAX_REGIONS>> = LazyInit::new();
pub fn init(boot_info: &BootInfo) {
    let mut regions = Vec::new();
    for_each_ram_region(boot_info, |start, size| {
        regions.push((start, size)).unwrap()
    });
    if regions.is_empty() {
        kplat::kprintln!(
            "boot memory map empty, protocol={:?}, fallback to config: base={:#x}, size={:#x}",
            boot_info.protocol(),
            PHYS_MEM_BASE,
            PHYS_MEM_SIZE
        );
        regions.push((PHYS_MEM_BASE, PHYS_MEM_SIZE)).unwrap();
    } else {
        kplat::kprintln!(
            "{:?} memory regions: {}",
            boot_info.protocol(),
            regions.len()
        );
    }
    RAM_REGIONS.init_once(regions);
}

struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    /// Returns all physical memory (RAM) ranges on the platform.
    fn ram_regions() -> &'static [MemRange] {
        RAM_REGIONS.as_slice()
    }

    fn rsvd_regions() -> &'static [MemRange] {
        &[(0, 0x100000)]
    }

    fn dma_regions() -> &'static [MemRange] {
        &[(kbuild_config::DMA_MEM_BASE, kbuild_config::DMA_MEM_SIZE)]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        default_p2v(paddr)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        default_v2p(vaddr)
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (
            va!(kbuild_config::KERNEL_ASPACE_BASE),
            kbuild_config::KERNEL_ASPACE_SIZE,
        )
    }
}
