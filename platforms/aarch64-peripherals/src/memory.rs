// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared AArch64 firmware memory helpers.

use core::sync::atomic::{AtomicUsize, Ordering};

use heapless::Vec;
use kplat::memory::{
    MemRange, PhysAddr, ReservedKind, ReservedRegion, ReservedSource, VirtAddr, sub_ranges,
};
use ktypes::Once;
use memaddr::MemoryAddr;
use of::{dtb_total_size_from_ptr, read_memory_regions, read_reserved_memory_regions};

const FIRMWARE_RESERVED_NAME: &str = "firmware reserved";

pub struct Aarch64MemState<const RAM: usize, const FW: usize> {
    ram_count: AtomicUsize,
    ram_regions: Once<[MemRange; RAM]>,
    fw_reserved_count: AtomicUsize,
    fw_reserved_regions: Once<[ReservedRegion; FW]>,
}

impl<const RAM: usize, const FW: usize> Aarch64MemState<RAM, FW> {
    pub const fn new() -> Self {
        Self {
            ram_count: AtomicUsize::new(0),
            ram_regions: Once::new(),
            fw_reserved_count: AtomicUsize::new(0),
            fw_reserved_regions: Once::new(),
        }
    }

    pub fn init(&'static self, regions: [ReservedRegion; FW], count: usize) {
        self.init_with_exclusions(regions, count);
    }

    pub fn init_with_exclusions(&'static self, regions: [ReservedRegion; FW], count: usize) {
        assert!(count <= FW, "too many firmware reserved regions");

        let (ram_ranges, ram_count) = read_sorted_ram_regions::<RAM>();
        assert!(ram_count != 0, "no RAM regions found in device tree");

        self.ram_regions.call_once(|| ram_ranges);
        self.ram_count.store(ram_count, Ordering::SeqCst);
        self.fw_reserved_regions.call_once(|| regions);
        self.fw_reserved_count.store(count, Ordering::SeqCst);
    }

    pub fn ram_regions(&'static self) -> &'static [MemRange] {
        self.ram_regions
            .get()
            .map(|ranges| &ranges[..self.ram_count.load(Ordering::Relaxed)])
            .expect("RAM regions are not initialized")
    }

    pub fn firmware_reserved_regions(&'static self) -> &'static [ReservedRegion] {
        let count = self.fw_reserved_count.load(Ordering::Relaxed);
        if count == 0 {
            return &[];
        }
        &self
            .fw_reserved_regions
            .get()
            .expect("firmware reserved regions are not initialized")[..count]
    }
}

impl<const RAM: usize, const FW: usize> Default for Aarch64MemState<RAM, FW> {
    fn default() -> Self {
        Self::new()
    }
}

pub fn read_sorted_ram_regions<const N: usize>() -> ([MemRange; N], usize) {
    let (regions, ram_count) = read_memory_regions::<N>();
    let mut ram_ranges = regions.map(|region| (region.starting_address as usize, region.size));
    if ram_count != 0 {
        ram_ranges[..ram_count].sort_unstable_by_key(|&(start, _)| start);
    }
    (ram_ranges, ram_count)
}

pub fn dtb_reserved_range(dtb_paddr: usize, p2v: fn(PhysAddr) -> VirtAddr) -> Option<MemRange> {
    if dtb_paddr == 0 {
        return None;
    }

    let dtb_va = p2v(PhysAddr::from_usize(dtb_paddr));
    let dtb_size = unsafe { dtb_total_size_from_ptr(dtb_va.as_usize() as *const u8) }.ok()?;
    let dtb_base = dtb_paddr.align_down_4k();
    let dtb_end = dtb_paddr.checked_add(dtb_size)?.align_up_4k();
    Some((dtb_base, dtb_end - dtb_base))
}

pub fn dtb_reserved_region(
    dtb_paddr: usize,
    p2v: fn(PhysAddr) -> VirtAddr,
) -> Option<ReservedRegion> {
    let (start, size) = dtb_reserved_range(dtb_paddr, p2v)?;
    Some(firmware_reserved_region(start, size))
}

pub fn firmware_reserved_region(start: usize, size: usize) -> ReservedRegion {
    ReservedRegion::new(
        start,
        size,
        ReservedKind::Firmware,
        ReservedSource::DeviceTree,
        FIRMWARE_RESERVED_NAME,
    )
}

pub fn collect_firmware_reserved_regions<const N: usize>(
    dtb_paddr: usize,
    p2v: fn(PhysAddr) -> VirtAddr,
    extra_regions: &[ReservedRegion],
) -> ([ReservedRegion; N], usize) {
    let mut reserved = Vec::<ReservedRegion, N>::new();

    for &region in extra_regions {
        push_reserved_non_overlapping(&mut reserved, region);
    }

    let (dtb_regions, count) = read_reserved_memory_regions::<N>();
    for &region in &dtb_regions[..count] {
        push_reserved_non_overlapping(
            &mut reserved,
            firmware_reserved_region(region.starting_address as usize, region.size),
        );
    }

    if let Some(region) = dtb_reserved_region(dtb_paddr, p2v) {
        push_reserved_non_overlapping(&mut reserved, region);
    }

    let count = reserved.len();
    let mut regions = [ReservedRegion::EMPTY; N];
    for (idx, region) in reserved.into_iter().enumerate() {
        regions[idx] = region;
    }
    (regions, count)
}

fn push_reserved_non_overlapping<const N: usize>(
    regions: &mut Vec<ReservedRegion, N>,
    region: ReservedRegion,
) {
    if region.is_empty() {
        return;
    }
    let mut cut = Vec::<MemRange, N>::new();
    for existing in regions.iter() {
        cut.push(existing.range()).expect("too many cut ranges");
    }
    cut.sort_unstable_by_key(|&(start, _)| start);
    let single = [region.range()];
    sub_ranges(&single, cut.as_slice(), |(start, size)| {
        regions
            .push(ReservedRegion::new(
                start,
                size,
                region.kind,
                region.source,
                region.name,
            ))
            .expect("too many firmware reserved ranges");
    })
    .expect("reserved overlap normalization failed");
    sort_and_merge_reserved(regions);
}

fn sort_and_merge_reserved<const N: usize>(regions: &mut Vec<ReservedRegion, N>) {
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
        } else {
            if next.start < cur_end {
                panic!("overlapping reserved regions with incompatible metadata");
            }
            write += 1;
            regions[write] = next;
        }
    }

    regions.truncate(write + 1);
}
