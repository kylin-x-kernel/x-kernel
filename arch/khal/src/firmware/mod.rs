// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "x86_64")]
use boot_info::BootInfo;
use memaddr::MemoryAddr;

use crate::mem::{self, MemRange, ReservedKind, ReservedRegion, ReservedSource};

mod init;
mod memory_source;
mod state;

const CMDLINE_BUF_SIZE: usize = 2048;
const DTB_CAPTURE_SIZE: usize = 0x20_0000;
const MAX_MEMORY_RAM_REGIONS: usize = 128;
const MAX_MEMORY_RESERVED_REGIONS: usize = 128;
const FIRMWARE_RESERVED_NAME: &str = "firmware reserved";

pub use state::{cmdline, dtb_capture_region};

pub fn init(boot_info: &boot_info::BootInfo) {
    init::init(boot_info);
}

pub fn init_memory_description(boot_info: &boot_info::BootInfo) {
    memory_source::init_memory_description(boot_info);
}

pub fn init_platform_memory_description(
    ram_regions: &[MemRange],
    reserved_regions: &[ReservedRegion],
) {
    memory_source::init_platform_memory_description(ram_regions, reserved_regions);
}

fn dtb_reserved_region(dtb_paddr: usize) -> Option<ReservedRegion> {
    if dtb_paddr == 0 {
        return None;
    }
    let dtb_size = of::dtb_total_size()?;
    let start = dtb_paddr.align_down_4k();
    let end = dtb_paddr.checked_add(dtb_size)?.align_up_4k();
    Some(firmware_reserved_region(start, end - start))
}

fn firmware_reserved_region(start: usize, size: usize) -> ReservedRegion {
    firmware_reserved_region_named(start, size, FIRMWARE_RESERVED_NAME)
}

fn firmware_reserved_region_named(start: usize, size: usize, name: &'static str) -> ReservedRegion {
    ReservedRegion::new(
        start,
        size,
        ReservedKind::Firmware,
        ReservedSource::DeviceTree,
        name,
    )
}

#[cfg(target_arch = "x86_64")]
fn x86_legacy_low_memory_region(_boot_info: &BootInfo) -> ReservedRegion {
    ReservedRegion::new(
        0,
        0x200000,
        ReservedKind::Platform,
        ReservedSource::Platform,
        "legacy low memory",
    )
}

fn push_reserved_non_overlapping<const N: usize>(
    regions: &mut heapless::Vec<ReservedRegion, N>,
    region: ReservedRegion,
) {
    if region.is_empty() {
        return;
    }

    let mut cut = heapless::Vec::<MemRange, N>::new();
    for existing in regions.iter() {
        cut.push(existing.range()).expect("too many cut ranges");
    }
    cut.sort_unstable_by_key(|&(start, _)| start);

    let single = [region.range()];
    mem::sub_ranges(&single, cut.as_slice(), |(start, size)| {
        regions
            .push(ReservedRegion::new(
                start,
                size,
                region.kind,
                region.source,
                region.name,
            ))
            .expect("too many reserved regions");
    })
    .expect("reserved overlap normalization failed");

    sort_and_merge_reserved(regions);
}

fn sort_and_merge_ranges<const N: usize>(regions: &mut heapless::Vec<MemRange, N>) {
    regions.sort_unstable_by_key(|&(start, _)| start);
    if regions.len() < 2 {
        return;
    }

    let mut write = 0;
    for read in 1..regions.len() {
        let (cur_start, cur_size) = regions[write];
        let cur_end = checked_range_end(cur_start, cur_size);
        let (next_start, next_size) = regions[read];
        let next_end = checked_range_end(next_start, next_size);

        if next_start <= cur_end {
            regions[write] = (cur_start, cur_end.max(next_end) - cur_start);
        } else {
            write += 1;
            regions[write] = regions[read];
        }
    }

    regions.truncate(write + 1);
}

fn sort_and_merge_reserved<const N: usize>(regions: &mut heapless::Vec<ReservedRegion, N>) {
    regions.sort_unstable_by_key(|region| region.start);
    if regions.len() < 2 {
        return;
    }

    let mut write = 0;
    for read in 1..regions.len() {
        let cur = regions[write];
        let next = regions[read];
        let cur_end = checked_range_end(cur.start, cur.size);
        let next_end = checked_range_end(next.start, next.size);

        if next.start <= cur_end
            && cur.kind == next.kind
            && cur.source == next.source
            && cur.name == next.name
        {
            regions[write].size = cur_end.max(next_end) - cur.start;
        } else if next.start < cur_end {
            panic!("overlapping reserved regions with incompatible metadata");
        } else {
            write += 1;
            regions[write] = next;
        }
    }

    regions.truncate(write + 1);
}

fn checked_range_end(start: usize, size: usize) -> usize {
    start
        .checked_add(size)
        .expect("memory range overflows address space")
}
