// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, pa, va};

use crate::config::{
    devices::MMIO_RANGES,
    plat::{KERNEL_BASE_PADDR, PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE, PHYS_VIRT_OFFSET},
};
struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [MemRange] {
        // TODO: paser dtb to get the available memory ranges
        // We can't directly use `PHYS_MEMORY_BASE` here, because it may has been used by sbi.
        &[(
            KERNEL_BASE_PADDR,
            PHYS_MEMORY_BASE + PHYS_MEMORY_SIZE - KERNEL_BASE_PADDR,
        )]
    }

    fn rsvd_regions() -> &'static [MemRange] {
        &[]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn dma_regions() -> &'static [MemRange] {
        &[]
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
