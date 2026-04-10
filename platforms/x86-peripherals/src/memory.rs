// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use heapless::Vec;
use khal::mem::{MemRange, ReservedKind, ReservedRegion, ReservedSource, sub_ranges};
use ktypes::Once;

use crate::bootmem::{BootMemoryKind, for_each_memory_region};

pub struct PlatformMemoryRegions<const RAM: usize, const RSVD: usize, const MMIO: usize> {
    pub ram: Vec<MemRange, RAM>,
    pub reserved: Vec<ReservedRegion, RSVD>,
    pub mmio: Vec<MemRange, MMIO>,
}

pub struct X86MemState<const RAM: usize, const RSVD: usize, const MMIO: usize> {
    ram_regions: Once<Vec<MemRange, RAM>>,
    reserved_regions: Once<Vec<ReservedRegion, RSVD>>,
    mmio_regions: Once<Vec<MemRange, MMIO>>,
}

impl<const RAM: usize, const RSVD: usize, const MMIO: usize> X86MemState<RAM, RSVD, MMIO> {
    pub const fn new() -> Self {
        Self {
            ram_regions: Once::new(),
            reserved_regions: Once::new(),
            mmio_regions: Once::new(),
        }
    }

    pub fn init<F>(
        &'static self,
        boot_info: &BootInfo,
        initial_reserved: &[ReservedRegion],
        static_mmio: &[MemRange],
        populate_mmio: F,
    ) where
        F: FnOnce(&mut Vec<MemRange, MMIO>),
    {
        let regions = collect_platform_regions::<RAM, RSVD, MMIO, _>(
            boot_info,
            initial_reserved,
            static_mmio,
            populate_mmio,
        );

        self.ram_regions.call_once(|| regions.ram);
        self.reserved_regions.call_once(|| regions.reserved);
        self.mmio_regions.call_once(|| regions.mmio);
    }

    pub fn ram_regions(&'static self) -> &'static [MemRange] {
        self.ram_regions
            .get()
            .map(|ranges| ranges.as_slice())
            .expect("x86 RAM regions are not initialized")
    }

    pub fn firmware_reserved_regions(&'static self) -> &'static [ReservedRegion] {
        self.reserved_regions
            .get()
            .map(|regions| regions.as_slice())
            .expect("x86 reserved regions are not initialized")
    }

    pub fn mmio_regions(&'static self) -> &'static [MemRange] {
        self.mmio_regions
            .get()
            .map(|ranges| ranges.as_slice())
            .expect("x86 MMIO regions are not initialized")
    }
}

impl<const RAM: usize, const RSVD: usize, const MMIO: usize> Default
    for X86MemState<RAM, RSVD, MMIO>
{
    fn default() -> Self {
        Self::new()
    }
}

pub fn collect_boot_regions<const RAM: usize, const RSVD: usize>(
    boot_info: &BootInfo,
    initial_reserved: &[ReservedRegion],
) -> (Vec<MemRange, RAM>, Vec<ReservedRegion, RSVD>) {
    let mut ram_regions = Vec::new();
    let mut reserved_regions = Vec::new();

    for &region in initial_reserved {
        reserved_regions
            .push(region)
            .expect("too many reserved ranges");
    }
    sort_and_merge_reserved(&mut reserved_regions);

    for_each_memory_region(boot_info, |kind, start, size| match kind {
        BootMemoryKind::Ram => ram_regions
            .push((start, size))
            .expect("too many RAM ranges"),
        BootMemoryKind::Reserved { kind, name } => push_reserved_non_overlapping(
            &mut reserved_regions,
            ReservedRegion::new(start, size, kind, ReservedSource::BootProtocol, name),
        ),
    });

    if boot_info.boot_runtime_size() != 0 && boot_info.boot_runtime_paddr() != 0 {
        push_reserved_non_overlapping(
            &mut reserved_regions,
            ReservedRegion::new(
                boot_info.boot_runtime_paddr(),
                boot_info.boot_runtime_size(),
                ReservedKind::BootRuntime,
                ReservedSource::BootProtocol,
                "boot runtime",
            ),
        );
    }

    sort_and_merge_ranges(&mut ram_regions);
    sort_and_merge_reserved(&mut reserved_regions);

    assert!(
        !ram_regions.is_empty(),
        "boot protocol did not provide any RAM regions"
    );

    (ram_regions, reserved_regions)
}

pub fn collect_platform_regions<const RAM: usize, const RSVD: usize, const MMIO: usize, F>(
    boot_info: &BootInfo,
    initial_reserved: &[ReservedRegion],
    static_mmio: &[MemRange],
    populate_mmio: F,
) -> PlatformMemoryRegions<RAM, RSVD, MMIO>
where
    F: FnOnce(&mut Vec<MemRange, MMIO>),
{
    let (ram, reserved) = collect_boot_regions::<RAM, RSVD>(boot_info, initial_reserved);

    let mut mmio = Vec::new();
    for &range in static_mmio {
        push_merged_region(&mut mmio, range.0, range.1);
    }
    if let Some(local_apic) = ::acpi::find_local_apic_address_from_init() {
        push_merged_region(&mut mmio, local_apic, 0x1000);
    }
    if let Some(io_apic) = ::acpi::find_io_apic_from_init() {
        push_merged_region(&mut mmio, io_apic.address as usize, 0x1000);
    }
    populate_mmio(&mut mmio);

    let reserved = subtract_reserved_ranges::<RSVD>(&reserved, &mmio);

    PlatformMemoryRegions {
        ram,
        reserved,
        mmio,
    }
}

pub fn push_merged_region<const N: usize>(
    regions: &mut Vec<MemRange, N>,
    start: usize,
    size: usize,
) {
    if size == 0 {
        return;
    }

    regions.push((start, size)).expect("too many regions");
    sort_and_merge_ranges(regions);
}

pub fn subtract_ranges<const N: usize>(base: &[MemRange], cut: &[MemRange]) -> Vec<MemRange, N> {
    let mut filtered = Vec::new();
    sub_ranges(base, cut, |range| {
        filtered.push(range).expect("too many filtered ranges");
    })
    .expect("memory range overlap in subtraction");
    filtered
}

fn subtract_reserved_ranges<const N: usize>(
    base: &[ReservedRegion],
    cut: &[MemRange],
) -> Vec<ReservedRegion, N> {
    let mut filtered = Vec::new();
    for &region in base {
        let single = [region.range()];
        sub_ranges(&single, cut, |(start, size)| {
            filtered
                .push(ReservedRegion::new(
                    start,
                    size,
                    region.kind,
                    region.source,
                    region.name,
                ))
                .expect("too many filtered reserved ranges");
        })
        .expect("reserved/mmio overlap normalization failed");
    }
    sort_and_merge_reserved(&mut filtered);
    filtered
}

fn push_reserved_non_overlapping<const N: usize>(
    regions: &mut Vec<ReservedRegion, N>,
    region: ReservedRegion,
) {
    if region.size == 0 {
        return;
    }
    let mut cut = Vec::<MemRange, N>::new();
    for existing in regions.iter() {
        cut.push(existing.range()).expect("too many cut ranges");
    }
    cut.sort_unstable_by_key(|&(start, _)| start);
    let single = [region.range()];
    sub_ranges(&single, &cut, |(start, size)| {
        regions
            .push(ReservedRegion::new(
                start,
                size,
                region.kind,
                region.source,
                region.name,
            ))
            .expect("too many reserved ranges");
    })
    .expect("reserved overlap normalization failed");
    sort_and_merge_reserved(regions);
}

fn sort_and_merge_ranges<const N: usize>(regions: &mut Vec<MemRange, N>) {
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

fn sort_and_merge_reserved<const N: usize>(regions: &mut Vec<ReservedRegion, N>) {
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

fn checked_range_end(start: usize, size: usize) -> usize {
    start
        .checked_add(size)
        .expect("memory region overflow while merging x86 ranges")
}
