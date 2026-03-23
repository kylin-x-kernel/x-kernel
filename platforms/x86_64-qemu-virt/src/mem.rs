// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory layout definitions for x86_64-qemu-virt.

use boot_info::BootInfo;
use heapless::Vec;
use kbuild_config::MMIO_RANGES;
use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, default_p2v, default_v2p, va};
use lazyinit::LazyInit;
use pci::{BarInfo, Cam, HeaderType, MmioCam, PciRoot};
use x86_peripherals::bootmem::for_each_ram_region;

use crate::acpi;

const MAX_REGIONS: usize = 128;
const MAX_MMIO_RANGES: usize = 64;
static RAM_REGIONS: LazyInit<Vec<MemRange, MAX_REGIONS>> = LazyInit::new();
static MMIO_REGIONS: LazyInit<Vec<MemRange, MAX_MMIO_RANGES>> = LazyInit::new();
/// Initializes RAM region list from boot protocol memory descriptors.
pub fn init(boot_info: &BootInfo) {
    let mut regions = Vec::new();
    let mut mmio_regions = Vec::new();
    for &range in MMIO_RANGES.iter() {
        mmio_regions.push(range).unwrap();
    }

    if let Some(mcfg) = acpi::find_mcfg(boot_info.rsdp_addr) {
        let ecam_start = mcfg.base_address as usize + ((mcfg.start_bus as usize) << 20);
        let ecam_size = ((mcfg.end_bus as usize - mcfg.start_bus as usize) + 1) << 20;
        push_mmio_region(&mut mmio_regions, ecam_start, ecam_size);
        collect_pci_bar_regions(&mut mmio_regions, ecam_start, mcfg.start_bus, mcfg.end_bus);
    }

    for_each_ram_region(boot_info, |start, size| {
        push_ram_region(&mut regions, start, size)
    });
    if regions.is_empty() {
        kplat::kprintln!(
            "boot memory map empty, protocol={:?}, fallback to config: base={:#x}, size={:#x}",
            boot_info.protocol(),
            kbuild_config::PHYS_MEM_BASE,
            kbuild_config::PHYS_MEM_SIZE
        );
        regions
            .push((kbuild_config::PHYS_MEM_BASE, kbuild_config::PHYS_MEM_SIZE))
            .unwrap();
    } else {
        kplat::kprintln!(
            "{:?} memory regions: {}",
            boot_info.protocol(),
            regions.len()
        );
        for (idx, (start, size)) in regions.iter().enumerate() {
            kplat::kprintln!("  ram[{idx}] = {start:#x}..{:#x}", start + size);
        }
    }
    RAM_REGIONS.init_once(regions);
    MMIO_REGIONS.init_once(mmio_regions);
}

fn push_ram_region(regions: &mut Vec<MemRange, MAX_REGIONS>, start: usize, size: usize) {
    if size == 0 {
        return;
    }

    if let Some((prev_start, prev_size)) = regions.last_mut()
        && *prev_start + *prev_size == start
    {
        *prev_size += size;
        return;
    }

    regions.push((start, size)).unwrap();
}

fn push_mmio_region(regions: &mut Vec<MemRange, MAX_MMIO_RANGES>, start: usize, size: usize) {
    if size == 0 {
        return;
    }

    let mut merged_start = start;
    let mut merged_end = start + size;
    let mut index = 0;
    while index < regions.len() {
        let (existing_start, existing_size) = regions[index];
        let existing_end = existing_start + existing_size;
        if merged_end < existing_start || merged_start > existing_end {
            index += 1;
            continue;
        }

        merged_start = merged_start.min(existing_start);
        merged_end = merged_end.max(existing_end);
        regions.swap_remove(index);
    }

    regions
        .push((merged_start, merged_end - merged_start))
        .unwrap();
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
                    push_mmio_region(regions, address as usize, size as usize);
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
        RAM_REGIONS.as_slice()
    }

    fn rsvd_regions() -> &'static [MemRange] {
        &[(0, 0x200000)]
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        MMIO_REGIONS.as_slice()
    }

    fn dma_regions() -> &'static [MemRange] {
        &[(kbuild_config::DMA_MEM_BASE, kbuild_config::DMA_MEM_SIZE)]
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
