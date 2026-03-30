// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified reserved-memory collection and normalization.

use heapless::Vec;
use kplat::memory::{
    MemRange, ReservedKind, ReservedRegion, ReservedSource, dma_regions, firmware_reserved_regions,
    sub_ranges,
};

const MAX_RESERVED_REGIONS: usize = 128;

pub fn reserved_regions(
    kernel_start: usize,
    kernel_size: usize,
) -> Vec<ReservedRegion, MAX_RESERVED_REGIONS> {
    let exclusions = exclusion_ranges(kernel_start, kernel_size);
    let mut reserved = Vec::new();

    for &region in firmware_reserved_regions() {
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

    for &(start, size) in dma_regions() {
        push_reserved_region(
            &mut reserved,
            ReservedRegion::new(
                start,
                size,
                ReservedKind::Dma,
                ReservedSource::Kernel,
                "dma",
            ),
        );
    }

    reserved
}

fn exclusion_ranges(
    kernel_start: usize,
    kernel_size: usize,
) -> Vec<MemRange, MAX_RESERVED_REGIONS> {
    let mut exclusions = Vec::new();
    exclusions
        .push((kernel_start, kernel_size))
        .expect("too many exclusion ranges");
    for &range in dma_regions() {
        exclusions.push(range).expect("too many exclusion ranges");
    }
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
