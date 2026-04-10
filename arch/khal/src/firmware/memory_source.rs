// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use heapless::Vec;
#[cfg(target_arch = "x86_64")]
use {
    boot_info::{BootProtocol, LinuxBootParams, X86LinuxE820EntryType},
    core::{mem as core_mem, ptr},
    multiboot2::{BootInformation, BootInformationHeader, MemoryAreaType},
};

#[cfg(target_arch = "x86_64")]
use super::x86_legacy_low_memory_region;
use super::{
    MAX_MEMORY_RAM_REGIONS, MAX_MEMORY_RESERVED_REGIONS, dtb_reserved_region,
    firmware_reserved_region, push_reserved_non_overlapping, sort_and_merge_ranges,
};
use crate::mem::{MemRange, ReservedKind, ReservedRegion, ReservedSource, init_described_regions};

pub(super) fn init_memory_description(boot_info: &BootInfo) {
    let mut ram_regions = Vec::<MemRange, MAX_MEMORY_RAM_REGIONS>::new();
    let mut reserved_regions = Vec::<ReservedRegion, MAX_MEMORY_RESERVED_REGIONS>::new();

    if boot_info.dtb_addr != 0 {
        collect_device_tree_memory(boot_info, &mut ram_regions, &mut reserved_regions);
    } else {
        collect_boot_protocol_memory(boot_info, &mut ram_regions, &mut reserved_regions);
    }

    sort_and_merge_ranges(&mut ram_regions);

    assert!(
        !ram_regions.is_empty(),
        "firmware did not provide any RAM regions"
    );
    init_described_regions(ram_regions.as_slice(), reserved_regions.as_slice());
}

fn collect_device_tree_memory(
    boot_info: &BootInfo,
    ram_regions: &mut Vec<MemRange, MAX_MEMORY_RAM_REGIONS>,
    reserved_regions: &mut Vec<ReservedRegion, MAX_MEMORY_RESERVED_REGIONS>,
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

    let (reserved, count) = of::read_reserved_memory_regions::<MAX_MEMORY_RESERVED_REGIONS>();
    for &region in &reserved[..count] {
        push_reserved_non_overlapping(
            reserved_regions,
            firmware_reserved_region(region.starting_address as usize, region.size),
        );
    }

    if let Some(region) = dtb_reserved_region(boot_info.dtb_addr) {
        push_reserved_non_overlapping(reserved_regions, region);
    }

    if let Some(reg) = of::dice_region() {
        push_reserved_non_overlapping(
            reserved_regions,
            ReservedRegion::new(
                reg.starting_address as usize,
                reg.size,
                ReservedKind::DevicePrivate,
                ReservedSource::DeviceTree,
                "dice",
            ),
        );
    }

    if boot_info.boot_runtime_size() != 0 && boot_info.boot_runtime_paddr() != 0 {
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

fn collect_boot_protocol_memory(
    boot_info: &BootInfo,
    ram_regions: &mut Vec<MemRange, MAX_MEMORY_RAM_REGIONS>,
    reserved_regions: &mut Vec<ReservedRegion, MAX_MEMORY_RESERVED_REGIONS>,
) {
    #[cfg(target_arch = "x86_64")]
    {
        push_reserved_non_overlapping(reserved_regions, x86_legacy_low_memory_region(boot_info));
        for_each_x86_boot_protocol_region(boot_info, |kind, start, size| match kind {
            X86BootMemoryKind::Ram => ram_regions
                .push((start, size))
                .expect("too many boot-protocol RAM regions"),
            X86BootMemoryKind::Reserved { kind, name } => push_reserved_non_overlapping(
                reserved_regions,
                ReservedRegion::new(start, size, kind, ReservedSource::BootProtocol, name),
            ),
        });

        if boot_info.boot_runtime_size() != 0 && boot_info.boot_runtime_paddr() != 0 {
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

    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (boot_info, ram_regions, reserved_regions);
        panic!("non-device-tree platform memory description is not initialized");
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum X86BootMemoryKind {
    Ram,
    Reserved {
        kind: ReservedKind,
        name: &'static str,
    },
}

#[cfg(target_arch = "x86_64")]
fn for_each_x86_boot_protocol_region(
    boot_info: &BootInfo,
    f: impl FnMut(X86BootMemoryKind, usize, usize),
) {
    match boot_info.protocol() {
        BootProtocol::Multiboot2 => {
            for_each_multiboot_region(boot_info.protocol_info_addr, f);
        }
        BootProtocol::Uefi => {
            for_each_uefi_region(boot_info.protocol_info_addr, f);
        }
        BootProtocol::LinuxBoot => {
            for_each_linuxboot_region(boot_info.protocol_info_addr, f);
        }
        _ => {}
    }
}

#[cfg(target_arch = "x86_64")]
fn for_each_multiboot_region(
    multiboot_info_ptr: usize,
    mut f: impl FnMut(X86BootMemoryKind, usize, usize),
) {
    if multiboot_info_ptr == 0 {
        return;
    }
    let info = unsafe { BootInformation::load(multiboot_info_ptr as *const BootInformationHeader) }
        .expect("invalid multiboot2 boot information");
    if let Some(mmap) = info.memory_map_tag() {
        for region in mmap.memory_areas() {
            let kind = match MemoryAreaType::from(region.typ()) {
                MemoryAreaType::Available => X86BootMemoryKind::Ram,
                MemoryAreaType::AcpiAvailable | MemoryAreaType::ReservedHibernate => {
                    X86BootMemoryKind::Reserved {
                        kind: ReservedKind::Acpi,
                        name: multiboot_reserved_name(MemoryAreaType::from(region.typ())),
                    }
                }
                MemoryAreaType::Defective => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: "defective",
                },
                other => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Firmware,
                    name: multiboot_reserved_name(other),
                },
            };
            f(
                kind,
                region.start_address() as usize,
                region.size() as usize,
            );
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn for_each_linuxboot_region(
    params_addr: usize,
    mut f: impl FnMut(X86BootMemoryKind, usize, usize),
) {
    let Some(params) = LinuxBootParams::new(params_addr) else {
        return;
    };

    for index in 0..params.e820_entries() {
        let entry = params
            .e820_entry(index)
            .expect("linux boot params e820 entry out of range");
        let kind = match entry.entry_type {
            x if x == X86LinuxE820EntryType::Ram as u32 => X86BootMemoryKind::Ram,
            x if x == X86LinuxE820EntryType::Acpi as u32
                || x == X86LinuxE820EntryType::Nvs as u32 =>
            {
                X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Acpi,
                    name: linuxboot_reserved_name(entry.entry_type),
                }
            }
            x if x == X86LinuxE820EntryType::Persistent as u32 => X86BootMemoryKind::Reserved {
                kind: ReservedKind::Persistent,
                name: linuxboot_reserved_name(entry.entry_type),
            },
            x if x == X86LinuxE820EntryType::Unusable as u32
                || x == X86LinuxE820EntryType::Disabled as u32 =>
            {
                X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: linuxboot_reserved_name(entry.entry_type),
                }
            }
            x if x == X86LinuxE820EntryType::SoftReserved as u32 => X86BootMemoryKind::Reserved {
                kind: ReservedKind::Platform,
                name: linuxboot_reserved_name(entry.entry_type),
            },
            _ => X86BootMemoryKind::Reserved {
                kind: ReservedKind::Firmware,
                name: linuxboot_reserved_name(entry.entry_type),
            },
        };
        f(kind, entry.addr as usize, entry.size as usize);
    }
}

#[cfg(target_arch = "x86_64")]
fn for_each_uefi_region(
    multiboot_info_ptr: usize,
    mut f: impl FnMut(X86BootMemoryKind, usize, usize),
) {
    if multiboot_info_ptr == 0 {
        return;
    }

    let info = unsafe { ptr::read_unaligned(multiboot_info_ptr as *const UefiMbInfo) };
    if (info.flags & (1 << 6)) == 0 || info.mmap_addr == 0 || info.mmap_length == 0 {
        return;
    }

    let mut cursor = info.mmap_addr as usize;
    let end = cursor + info.mmap_length as usize;
    while cursor + core_mem::size_of::<UefiMbMmapEntry>() <= end {
        let entry = unsafe { ptr::read_unaligned(cursor as *const UefiMbMmapEntry) };
        if entry.len != 0 {
            let kind = match entry.typ {
                7 => X86BootMemoryKind::Ram,
                9 | 10 => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Acpi,
                    name: uefi_reserved_name(entry.typ),
                },
                14 => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Persistent,
                    name: uefi_reserved_name(entry.typ),
                },
                8 | 15 => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: uefi_reserved_name(entry.typ),
                },
                _ => X86BootMemoryKind::Reserved {
                    kind: ReservedKind::Firmware,
                    name: uefi_reserved_name(entry.typ),
                },
            };
            f(kind, entry.addr as usize, entry.len as usize);
        }

        let entry_size = entry.size as usize + core_mem::size_of::<u32>();
        if entry_size == 0 {
            break;
        }
        cursor += entry_size;
    }
}

#[cfg(target_arch = "x86_64")]
fn multiboot_reserved_name(kind: MemoryAreaType) -> &'static str {
    match kind {
        MemoryAreaType::Available => "available",
        MemoryAreaType::Reserved => "reserved",
        MemoryAreaType::AcpiAvailable => "acpi reclaimable",
        MemoryAreaType::ReservedHibernate => "acpi nvs",
        MemoryAreaType::Defective => "defective",
        _ => "firmware reserved",
    }
}

#[cfg(target_arch = "x86_64")]
fn linuxboot_reserved_name(kind: u32) -> &'static str {
    match kind {
        x if x == X86LinuxE820EntryType::Reserved as u32 => "reserved",
        x if x == X86LinuxE820EntryType::Acpi as u32 => "acpi reclaimable",
        x if x == X86LinuxE820EntryType::Nvs as u32 => "acpi nvs",
        x if x == X86LinuxE820EntryType::Persistent as u32 => "persistent",
        x if x == X86LinuxE820EntryType::Unusable as u32 => "unusable",
        x if x == X86LinuxE820EntryType::Disabled as u32 => "disabled",
        x if x == X86LinuxE820EntryType::SoftReserved as u32 => "soft reserved",
        _ => "firmware reserved",
    }
}

#[cfg(target_arch = "x86_64")]
fn uefi_reserved_name(kind: u32) -> &'static str {
    match kind {
        0 => "reserved",
        1 => "loader code",
        2 => "loader data",
        3 => "boot services code",
        4 => "boot services data",
        5 => "runtime services code",
        6 => "runtime services data",
        8 => "unusable",
        9 => "acpi reclaimable",
        10 => "acpi nvs",
        11 => "mmio",
        12 => "mmio port space",
        13 => "pal code",
        14 => "persistent",
        15 => "unaccepted",
        _ => "firmware reserved",
    }
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct UefiMbInfo {
    flags: u32,
    mem_lower: u32,
    mem_upper: u32,
    boot_device: u32,
    cmdline: u32,
    mods_count: u32,
    mods_addr: u32,
    syms: [u32; 4],
    mmap_length: u32,
    mmap_addr: u32,
    drives_length: u32,
    drives_addr: u32,
    config_table: u32,
    boot_loader_name: u32,
    apm_table: u32,
    vbe_control_info: u32,
    vbe_mode_info: u32,
    vbe_mode: u16,
    vbe_interface_seg: u16,
    vbe_interface_off: u16,
    vbe_interface_len: u16,
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Clone, Copy)]
struct UefiMbMmapEntry {
    size: u32,
    addr: u64,
    len: u64,
    typ: u32,
}
