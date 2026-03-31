// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use kbuild_config::MMIO_RANGES;
use kplat::memory::{
    HwMemory, MemRange, PhysAddr, ReservedKind, ReservedRegion, ReservedSource, VirtAddr,
    default_p2v, default_v2p, va,
};
use x86_peripherals::memory::X86MemState;

const MAX_REGIONS: usize = 128;
static MEM_STATE: X86MemState<MAX_REGIONS, MAX_REGIONS, MAX_REGIONS> = X86MemState::new();
pub fn init(boot_info: &BootInfo) {
    MEM_STATE.init(
        boot_info,
        &[ReservedRegion::new(
            0,
            0x100000,
            ReservedKind::Platform,
            ReservedSource::Platform,
            "legacy low memory",
        )],
        MMIO_RANGES,
        |_| {},
    );
    kplat::kprintln!(
        "{:?} memory regions: {}",
        boot_info.protocol(),
        MEM_STATE.ram_regions().len()
    );
}

struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    /// Returns all physical memory (RAM) ranges on the platform.
    fn ram_regions() -> &'static [MemRange] {
        MEM_STATE.ram_regions()
    }

    fn firmware_reserved_regions() -> &'static [ReservedRegion] {
        MEM_STATE.firmware_reserved_regions()
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        MEM_STATE.mmio_regions()
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
