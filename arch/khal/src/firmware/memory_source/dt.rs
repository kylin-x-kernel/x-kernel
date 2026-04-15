// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;

use super::{
    super::firmware_reserved_region_named, MAX_MEMORY_RAM_REGIONS, MAX_MEMORY_RESERVED_REGIONS,
    RamRegions, ReservedRegions, append_boot_runtime_reserved, append_dtb_reserved,
    push_reserved_non_overlapping, sort_and_merge_ranges,
};

pub(super) fn collect_from_global(
    boot_info: &BootInfo,
    ram_regions: &mut RamRegions,
    reserved_regions: &mut ReservedRegions,
) {
    collect_initialized_regions(ram_regions, reserved_regions);
    append_dtb_reserved(boot_info, reserved_regions);
    append_boot_runtime_reserved(boot_info, reserved_regions);
}

pub(super) fn collect_initialized_regions(
    ram_regions: &mut RamRegions,
    reserved_regions: &mut ReservedRegions,
) {
    let (regions, count) = of::read_memory_regions::<MAX_MEMORY_RAM_REGIONS>();
    for region in &regions[..count] {
        if region.size == 0 {
            continue;
        }
        ram_regions
            .push((region.starting_address as usize, region.size))
            .expect("too many device tree RAM regions");
    }
    sort_and_merge_ranges(ram_regions);

    let (reserved, count) = of::read_named_reserved_memory_regions::<MAX_MEMORY_RESERVED_REGIONS>();
    for region in &reserved[..count] {
        push_reserved_non_overlapping(
            reserved_regions,
            firmware_reserved_region_named(
                region.region.starting_address as usize,
                region.region.size,
                region.name,
            ),
        );
    }
}
