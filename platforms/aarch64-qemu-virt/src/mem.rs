// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory layout definitions for aarch64-qemu-virt.

use aarch64_peripherals::memory::{Aarch64MemState, collect_firmware_reserved_regions};
use kbuild_config::MMIO_RANGES;
use kplat::memory::{
    HwMemory, MemRange, PhysAddr, ReservedRegion, VirtAddr, default_p2v, default_v2p, va,
};

// Platform runtime RAM-region capacity. Boot-time DTB parsing has its own cap
// in `boot/kernel-boot/.../mmu.rs`; keep them aligned conceptually so both
// stages accept the same class of DTBs.
const MAX_RAM_REGIONS: usize = 8;
const MAX_FW_RESERVED_REGIONS: usize = 32;
static MEM_STATE: Aarch64MemState<MAX_RAM_REGIONS, MAX_FW_RESERVED_REGIONS> =
    Aarch64MemState::new();

pub(crate) fn early_init(dtb_paddr: usize, kernel_load_paddr: usize) {
    let (regions, count) =
        collect_firmware_reserved_regions::<MAX_FW_RESERVED_REGIONS>(dtb_paddr, default_p2v, &[]);
    let _ = kernel_load_paddr;
    MEM_STATE.init_with_exclusions(regions, count);
}

struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [MemRange] {
        MEM_STATE.ram_regions()
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Reserved memory can be contained in [`ram_regions`], they are not
    /// allocatable but should be mapped to kernel's address space.
    fn firmware_reserved_regions() -> &'static [ReservedRegion] {
        MEM_STATE.firmware_reserved_regions()
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
