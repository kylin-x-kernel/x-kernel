// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory layout definitions for aarch64-qemu-virt.

use kbuild_config::{MMIO_RANGES, PHYS_MEM_BASE, PHYS_MEM_SIZE, PHYS_VIRT_OFFSET};
use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, pa, va};
struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [MemRange] {
        &[(PHYS_MEM_BASE, PHYS_MEM_SIZE)]
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Reserved memory can be contained in [`ram_regions`], they are not
    /// allocatable but should be mapped to kernel's address space.
    fn rsvd_regions() -> &'static [MemRange] {
        &[]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn dma_regions() -> &'static [MemRange] {
        &[(kbuild_config::DMA_MEM_BASE, kbuild_config::DMA_MEM_SIZE)]
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (
            va!(kbuild_config::KERNEL_ASPACE_BASE),
            kbuild_config::KERNEL_ASPACE_SIZE,
        )
    }
}
