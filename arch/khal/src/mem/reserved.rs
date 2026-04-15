// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use heapless::Vec;

use super::{
    MemRange, MemoryRegion, ReservedKind, ReservedRegion, ReservedSource,
    described_reserved_regions, sub_ranges,
};

const MAX_RESERVED_REGIONS: usize = 128;

pub fn reserved_regions(
    kernel_start: usize,
    kernel_size: usize,
) -> Vec<ReservedRegion, MAX_RESERVED_REGIONS> {
    let exclusions = exclusion_ranges(kernel_start, kernel_size);
    let mut reserved = Vec::new();

    for &region in described_reserved_regions() {
        let single = [region.range()];
        sub_ranges(&single, &exclusions, |(start, size)| {
            push_reserved_region(
                &mut reserved,
                ReservedRegion::new(start, size, region.kind, region.source, region.name),
            );
        })
        .inspect_err(|(a, b)| {
            error!("Firmware reserved memory region {a:#x?} overlaps with exclusion {b:#x?}")
        })
        .unwrap();
    }

    push_reserved_region(
        &mut reserved,
        ReservedRegion::new(
            kernel_start,
            kernel_size,
            ReservedKind::KernelImage,
            ReservedSource::Kernel,
            "kernel image",
        ),
    );

    reserved
}

pub fn append_reserved_memory_regions(
    reserved: &[ReservedRegion],
    mut push: impl FnMut(MemoryRegion),
) -> Vec<MemRange, MAX_RESERVED_REGIONS> {
    let mut reserved_ranges = Vec::new();
    for &region in reserved {
        if !matches!(region.kind, ReservedKind::KernelImage) {
            push(MemoryRegion::new_rsvd(
                region.start,
                region.size,
                region.name,
            ));
        }
        reserved_ranges
            .push(region.range())
            .expect("too many reserved ranges");
    }
    reserved_ranges.sort_unstable_by_key(|&(start, _)| start);
    reserved_ranges
}

pub fn describe_reserved_memory_region(region: &MemoryRegion) -> Option<ReservedRegion> {
    let start = region.paddr.as_usize();
    let end = start.checked_add(region.size)?;

    described_reserved_regions()
        .iter()
        .copied()
        .find(|reserved| {
            let reserved_end = reserved.start + reserved.size;
            reserved.name == region.name && start >= reserved.start && end <= reserved_end
        })
        .or_else(|| {
            (region.name == "kernel image").then_some(ReservedRegion::new(
                start,
                region.size,
                ReservedKind::KernelImage,
                ReservedSource::Kernel,
                "kernel image",
            ))
        })
}

fn exclusion_ranges(
    kernel_start: usize,
    kernel_size: usize,
) -> Vec<MemRange, MAX_RESERVED_REGIONS> {
    let mut exclusions = Vec::new();
    exclusions
        .push((kernel_start, kernel_size))
        .expect("too many exclusion ranges");
    exclusions.sort_unstable_by_key(|&(start, _)| start);
    exclusions
}

fn push_reserved_region<const N: usize>(
    regions: &mut Vec<ReservedRegion, N>,
    region: ReservedRegion,
) {
    if region.is_empty() {
        return;
    }

    regions.push(region).expect("too many reserved regions");
    regions.sort_unstable_by_key(|region| region.start);
    if regions.len() < 2 {
        return;
    }

    let mut write = 0;
    for read in 1..regions.len() {
        let cur = regions[write];
        let next = regions[read];
        let cur_end = cur.start + cur.size;
        let next_end = next.start + next.size;

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
