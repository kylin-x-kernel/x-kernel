// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory layout definitions for x86_64-qemu-virt.

use boot_info::BootInfo;
use heapless::Vec;
use kbuild_config::MMIO_RANGES;
use kplat::memory::{
    HwMemory, MemRange, PhysAddr, ReservedKind, ReservedRegion, ReservedSource, VirtAddr,
    default_p2v, default_v2p,
};
use pci::{BarInfo, Cam, HeaderType, MmioCam, PciRoot};
use x86_peripherals::memory::{X86MemState, push_merged_region};

const MAX_REGIONS: usize = 128;
const MAX_MMIO_RANGES: usize = 64;
static MEM_STATE: X86MemState<MAX_REGIONS, MAX_REGIONS, MAX_MMIO_RANGES> = X86MemState::new();
/// Initializes RAM region list from boot protocol memory descriptors.
pub fn init(boot_info: &BootInfo) {
    MEM_STATE.init(
        boot_info,
        &[ReservedRegion::new(
            0,
            0x200000,
            ReservedKind::Platform,
            ReservedSource::Platform,
            "legacy low memory",
        )],
        MMIO_RANGES,
        |mmio_regions| {
            if let Some(mcfg) = ::acpi::find_mcfg_from_init()
                && let Some((ecam_start, ecam_size)) = mcfg.ecam_region()
            {
                push_merged_region(mmio_regions, ecam_start, ecam_size);
                collect_pci_bar_regions(
                    mmio_regions,
                    mcfg.base_address as usize,
                    mcfg.start_bus,
                    mcfg.end_bus,
                );
            }
        },
    );

    kplat::kprintln!(
        "{:?} memory regions: {}",
        boot_info.protocol(),
        MEM_STATE.ram_regions().len()
    );
    for (idx, (start, size)) in MEM_STATE.ram_regions().iter().enumerate() {
        kplat::kprintln!("  ram[{idx}] = {start:#x}..{:#x}", start + size);
    }
}

fn collect_pci_bar_regions(
    regions: &mut Vec<MemRange, MAX_MMIO_RANGES>,
    pci_config_base: usize,
    pci_bus_start: u8,
    pci_bus_end: u8,
) {
    let base_vaddr = default_p2v(pci_config_base.into());
    let cam = unsafe { MmioCam::new(base_vaddr.as_mut_ptr(), Cam::Ecam) };
    let mut root = PciRoot::new(cam);

    for bus in pci_bus_start..=pci_bus_end {
        for (bdf, dev_info) in root.enumerate_bus(bus) {
            if dev_info.header_type != HeaderType::Standard {
                continue;
            }

            let mut bar = 0;
            while bar < 6 {
                let info = root.bar_info(bdf, bar).unwrap();
                if let Some(BarInfo::Memory { address, size, .. }) = info
                    && address > 0
                    && size > 0
                {
                    push_merged_region(regions, address as usize, size as usize);
                }

                bar += 1;
                if info.as_ref().is_some_and(BarInfo::takes_two_entries) {
                    bar += 1;
                }
            }
        }
    }
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
}
