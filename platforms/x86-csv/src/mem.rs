// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use heapless::Vec;
use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, pa, va};
use lazyinit::LazyInit;
use multiboot::information::{MemoryManagement, MemoryType, Multiboot, PAddr};

use crate::config::{
    devices::MMIO_RANGES,
    plat::{PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE, PHYS_VIRT_OFFSET},
};
const MAX_REGIONS: usize = 16;
static RAM_REGIONS: LazyInit<Vec<MemRange, MAX_REGIONS>> = LazyInit::new();
pub fn init(multiboot_info_ptr: usize) {
    let mut mm = HwMemoryImpl;
    let info = unsafe { Multiboot::from_ptr(multiboot_info_ptr as _, &mut mm).unwrap() };
    let mut regions = Vec::new();
    for r in info.memory_regions().unwrap() {
        if r.memory_type() == MemoryType::Available {
            regions
                .push((r.base_address() as usize, r.length() as usize))
                .unwrap();
        }
    }
    if regions.is_empty() {
        kplat::kprintln!(
            "multiboot memory map empty, fallback to config: base={:#x}, size={:#x}",
            PHYS_MEMORY_BASE,
            PHYS_MEMORY_SIZE
        );
        regions
            .push((PHYS_MEMORY_BASE as usize, PHYS_MEMORY_SIZE as usize))
            .unwrap();
    } else {
        kplat::kprintln!("multiboot memory regions: {}", regions.len());
    }
    RAM_REGIONS.init_once(regions);
}
struct HwMemoryImpl;
impl MemoryManagement for HwMemoryImpl {
    unsafe fn paddr_to_slice(&self, addr: PAddr, size: usize) -> Option<&'static [u8]> {
        let ptr = Self::p2v(pa!(addr as usize)).as_ptr();
        Some(unsafe { core::slice::from_raw_parts(ptr, size) })
    }

    unsafe fn allocate(&mut self, _length: usize) -> Option<(PAddr, &mut [u8])> {
        None
    }

    unsafe fn deallocate(&mut self, _addr: PAddr) {}
}
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    /// Returns all physical memory (RAM) ranges on the platform.
    fn ram_regions() -> &'static [MemRange] {
        RAM_REGIONS.as_slice()
    }

    fn rsvd_regions() -> &'static [MemRange] {
        &[(0, 0x100000)]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (
            va!(crate::config::plat::KERNEL_ASPACE_BASE),
            crate::config::plat::KERNEL_ASPACE_SIZE,
        )
    }
}
