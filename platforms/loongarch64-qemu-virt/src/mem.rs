// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::memory::{HwMemory, PhysAddr, RawRange, VirtAddr, pa, va};

use crate::config::{
    devices::MMIO_RANGES,
    plat::{
        HIGH_MEMORY_BASE, LOW_MEMORY_BASE, LOW_MEMORY_SIZE, PHYS_BOOT_OFFSET, PHYS_MEMORY_SIZE,
        PHYS_VIRT_OFFSET,
    },
};
struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [RawRange] {
        const HIGH_MEMORY_SIZE: usize = PHYS_MEMORY_SIZE.saturating_sub(LOW_MEMORY_SIZE);
        if HIGH_MEMORY_SIZE == 0 {
            &[(LOW_MEMORY_BASE, PHYS_MEMORY_SIZE)]
        } else {
            &[
                (LOW_MEMORY_BASE, LOW_MEMORY_SIZE),
                (HIGH_MEMORY_BASE, HIGH_MEMORY_SIZE),
            ]
        }
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Reserved memory can be contained in [`ram_regions`], they are not
    /// allocatable but should be mapped to kernel's address space.
    fn reserved_ram_regions() -> &'static [RawRange] {
        &[(0, 0x200000)] // boot_info + fdt
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [RawRange] {
        &MMIO_RANGES
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        let vaddr = vaddr.as_usize();
        if vaddr & 0xffff_0000_0000_0000 == PHYS_BOOT_OFFSET {
            pa!(vaddr - PHYS_BOOT_OFFSET)
        } else {
            pa!(vaddr - PHYS_VIRT_OFFSET)
        }
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (
            va!(crate::config::plat::KERNEL_ASPACE_BASE),
            crate::config::plat::KERNEL_ASPACE_SIZE,
        )
    }
}
