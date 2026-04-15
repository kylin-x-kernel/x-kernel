// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::{BootInfo, MemoryDescriptionRoot};
use heapless::Vec;

use super::{
    MAX_MEMORY_RAM_REGIONS, MAX_MEMORY_RESERVED_REGIONS, dtb_reserved_region,
    push_reserved_non_overlapping, sort_and_merge_ranges,
};
use crate::mem::{MemRange, ReservedKind, ReservedRegion, ReservedSource, init_described_regions};

mod dt;
#[cfg(not(target_arch = "x86_64"))]
mod efi;
#[cfg(target_arch = "x86_64")]
mod x86;

type RamRegions = Vec<MemRange, MAX_MEMORY_RAM_REGIONS>;
type ReservedRegions = Vec<ReservedRegion, MAX_MEMORY_RESERVED_REGIONS>;

pub(super) fn init_memory_description(boot_info: &BootInfo) {
    let mut ram_regions = RamRegions::new();
    let mut reserved_regions = ReservedRegions::new();

    match boot_info.memory_description_root() {
        MemoryDescriptionRoot::DeviceTree => {
            dt::collect_from_global(boot_info, &mut ram_regions, &mut reserved_regions);
        }
        #[cfg(not(target_arch = "x86_64"))]
        MemoryDescriptionRoot::UefiMemmap => {
            efi::collect_from_boot_protocol(boot_info, &mut ram_regions, &mut reserved_regions);
        }
        #[cfg(target_arch = "x86_64")]
        MemoryDescriptionRoot::X86BootProtocol => {
            x86::collect_from_boot_protocol(boot_info, &mut ram_regions, &mut reserved_regions);
        }
        #[cfg(target_arch = "x86_64")]
        MemoryDescriptionRoot::UefiMemmap => {
            unreachable!("x86_64 does not use generic UEFI memory source")
        }
        #[cfg(not(target_arch = "x86_64"))]
        MemoryDescriptionRoot::X86BootProtocol => {
            unreachable!("non-x86 target does not use x86 boot protocol memory source")
        }
        MemoryDescriptionRoot::Unknown => {
            panic!("boot handoff did not choose a memory description root")
        }
    }

    finalize_regions(ram_regions, reserved_regions);
}

pub(super) fn init_platform_memory_description(
    ram_regions: &[MemRange],
    reserved_regions: &[ReservedRegion],
) {
    assert!(
        !ram_regions.is_empty(),
        "platform did not provide any RAM regions"
    );
    init_described_regions(ram_regions, reserved_regions);
}

fn finalize_regions(mut ram_regions: RamRegions, reserved_regions: ReservedRegions) {
    sort_and_merge_ranges(&mut ram_regions);
    assert!(
        !ram_regions.is_empty(),
        "firmware did not provide any RAM regions"
    );
    init_described_regions(ram_regions.as_slice(), reserved_regions.as_slice());
}

fn append_dtb_reserved(boot_info: &BootInfo, reserved_regions: &mut ReservedRegions) {
    if let Some(region) = dtb_reserved_region(boot_info.dtb_addr) {
        push_reserved_non_overlapping(reserved_regions, region);
    }
}

fn append_boot_runtime_reserved(boot_info: &BootInfo, reserved_regions: &mut ReservedRegions) {
    if boot_info.boot_runtime_size() != 0 {
        push_reserved_non_overlapping(
            reserved_regions,
            ReservedRegion::new(
                boot_info.boot_runtime_paddr(),
                boot_info.boot_runtime_size(),
                ReservedKind::BootRuntime,
                ReservedSource::BootProtocol,
                "boot runtime",
            ),
        );
    }
}
