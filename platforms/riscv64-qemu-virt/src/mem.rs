// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use heapless::Vec;
use kbuild_config::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, MMIO_RANGES};
use kplat::memory::{
    HwMemory, MemRange, PhysAddr, ReservedKind, ReservedRegion, ReservedSource, VirtAddr,
    default_p2v, default_v2p, sub_ranges, va,
};
use ktypes::Once;
use of::{dtb_total_size_from_ptr, read_memory_regions, read_reserved_memory_regions};

const MAX_RAM_REGIONS: usize = 8;
const MAX_FW_RESERVED_REGIONS: usize = 32;
const FIRMWARE_RESERVED_NAME: &str = "firmware reserved";

struct RiscvMemState<const RAM: usize, const FW: usize> {
    ram_count: AtomicUsize,
    ram_regions: Once<[MemRange; RAM]>,
    fw_reserved_count: AtomicUsize,
    fw_reserved_regions: Once<[ReservedRegion; FW]>,
}

impl<const RAM: usize, const FW: usize> RiscvMemState<RAM, FW> {
    const fn new() -> Self {
        Self {
            ram_count: AtomicUsize::new(0),
            ram_regions: Once::new(),
            fw_reserved_count: AtomicUsize::new(0),
            fw_reserved_regions: Once::new(),
        }
    }

    fn init(&'static self, dtb_paddr: usize) {
        let (ram_ranges, ram_count) = read_sorted_ram_regions::<RAM>();
        assert!(ram_count != 0, "no RAM regions found in device tree");

        let (fw_reserved_regions, fw_reserved_count) =
            collect_firmware_reserved_regions::<FW>(dtb_paddr);

        self.ram_regions.call_once(|| ram_ranges);
        self.ram_count.store(ram_count, Ordering::SeqCst);
        self.fw_reserved_regions.call_once(|| fw_reserved_regions);
        self.fw_reserved_count
            .store(fw_reserved_count, Ordering::SeqCst);
    }

    fn ram_regions(&'static self) -> &'static [MemRange] {
        self.ram_regions
            .get()
            .map(|ranges| &ranges[..self.ram_count.load(Ordering::Relaxed)])
            .expect("RAM regions are not initialized")
    }

    fn firmware_reserved_regions(&'static self) -> &'static [ReservedRegion] {
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

static MEM_STATE: RiscvMemState<MAX_RAM_REGIONS, MAX_FW_RESERVED_REGIONS> = RiscvMemState::new();

pub(crate) fn early_init(dtb_paddr: usize, _kernel_load_paddr: usize) {
    MEM_STATE.init(dtb_paddr);
}

struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [MemRange] {
        MEM_STATE.ram_regions()
    }

    fn firmware_reserved_regions() -> &'static [ReservedRegion] {
        MEM_STATE.firmware_reserved_regions()
    }

    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        default_p2v(paddr)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        default_v2p(vaddr)
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (va!(KERNEL_ASPACE_BASE), KERNEL_ASPACE_SIZE)
    }
}

fn read_sorted_ram_regions<const N: usize>() -> ([MemRange; N], usize) {
    let (regions, count) = read_memory_regions::<N>();
    let mut ranges = regions.map(|region| (region.starting_address as usize, region.size));
    if count != 0 {
        ranges[..count].sort_unstable_by_key(|&(start, _)| start);
    }
    (ranges, count)
}

fn collect_firmware_reserved_regions<const N: usize>(
    dtb_paddr: usize,
) -> ([ReservedRegion; N], usize) {
    let mut reserved = Vec::<ReservedRegion, N>::new();

    let (dtb_regions, count) = read_reserved_memory_regions::<N>();
    for &region in &dtb_regions[..count] {
        push_reserved_non_overlapping(
            &mut reserved,
            firmware_reserved_region(region.starting_address as usize, region.size),
        );
    }

    if let Some(region) = dtb_reserved_region(dtb_paddr) {
        push_reserved_non_overlapping(&mut reserved, region);
    }

    let count = reserved.len();
    let mut regions = [ReservedRegion::EMPTY; N];
    for (idx, region) in reserved.into_iter().enumerate() {
        regions[idx] = region;
    }
    (regions, count)
}

fn dtb_reserved_region(dtb_paddr: usize) -> Option<ReservedRegion> {
    let dtb_va = default_p2v(PhysAddr::from_usize(dtb_paddr));
    let dtb_size = unsafe { dtb_total_size_from_ptr(dtb_va.as_usize() as *const u8) }.ok()?;
    let start = dtb_paddr & !0xfff;
    let end = (dtb_paddr.checked_add(dtb_size)? + 0xfff) & !0xfff;
    Some(firmware_reserved_region(start, end - start))
}

fn firmware_reserved_region(start: usize, size: usize) -> ReservedRegion {
    ReservedRegion::new(
        start,
        size,
        ReservedKind::Firmware,
        ReservedSource::DeviceTree,
        FIRMWARE_RESERVED_NAME,
    )
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
            if next.start <= cur_end {
                panic!("overlapping reserved regions with incompatible metadata");
            }
            write += 1;
            regions[write] = next;
        }
    }

    regions.truncate(write + 1);
}
