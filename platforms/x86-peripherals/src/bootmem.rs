// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{mem, ptr};

use boot_info::{BootInfo, BootProtocol, LinuxBootParams, X86LinuxE820EntryType};
use khal::mem::ReservedKind;
use klazy::Once;
use multiboot2::{BootInformation, BootInformationHeader, MemoryAreaType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMemoryKind {
    Ram,
    Reserved {
        kind: ReservedKind,
        name: &'static str,
    },
}

const AP_TRAMPOLINE_MIN_PADDR: usize = 0x1_0000;
const AP_TRAMPOLINE_MAX_PADDR: usize = 0xA_0000;

static AP_TRAMPOLINE_PAGE: Once<usize> = Once::new();

pub fn init_ap_trampoline_page(boot_info: &BootInfo) {
    let paddr = choose_ap_trampoline_page(boot_info)
        .expect("failed to find a low-memory x86 AP trampoline page");
    AP_TRAMPOLINE_PAGE.call_once(|| paddr);
}

pub fn ap_trampoline_page_paddr() -> usize {
    AP_TRAMPOLINE_PAGE
        .get()
        .copied()
        .expect("x86 AP trampoline page is not initialized")
}

pub fn for_each_ram_region<F>(boot_info: &BootInfo, mut f: F)
where
    F: FnMut(usize, usize),
{
    for_each_memory_region(boot_info, |kind, start, size| {
        if matches!(kind, BootMemoryKind::Ram) {
            f(start, size);
        }
    });
}

pub fn for_each_memory_region<F>(boot_info: &BootInfo, f: F)
where
    F: FnMut(BootMemoryKind, usize, usize),
{
    match boot_info.protocol() {
        BootProtocol::Multiboot2 => for_each_multiboot_region(boot_info.protocol_info_addr, f),
        BootProtocol::Uefi => for_each_uefi_region(boot_info.protocol_info_addr, f),
        BootProtocol::LinuxBoot => for_each_linuxboot_region(boot_info.protocol_info_addr, f),
        _ => {}
    }
}

fn choose_ap_trampoline_page(boot_info: &BootInfo) -> Option<usize> {
    let mut best = None;
    for_each_ram_region(boot_info, |start, size| {
        let Some(end) = start.checked_add(size) else {
            return;
        };
        let start = start.max(AP_TRAMPOLINE_MIN_PADDR);
        let end = end.min(AP_TRAMPOLINE_MAX_PADDR);
        if end <= start {
            return;
        }

        let Some(candidate_end) = end.checked_sub(0x1000) else {
            return;
        };
        let candidate = candidate_end & !0xfff;
        if candidate < start || candidate + 0x1000 > end {
            return;
        }
        best = Some(best.map_or(candidate, |current: usize| current.max(candidate)));
    });
    best
}

fn for_each_multiboot_region<F>(multiboot_info_ptr: usize, mut f: F)
where
    F: FnMut(BootMemoryKind, usize, usize),
{
    if multiboot_info_ptr == 0 {
        return;
    }
    let info = unsafe { BootInformation::load(multiboot_info_ptr as *const BootInformationHeader) }
        .expect("invalid multiboot2 boot information");
    if let Some(mmap) = info.memory_map_tag() {
        for region in mmap.memory_areas() {
            let kind = match MemoryAreaType::from(region.typ()) {
                MemoryAreaType::Available => BootMemoryKind::Ram,
                MemoryAreaType::AcpiAvailable | MemoryAreaType::ReservedHibernate => {
                    BootMemoryKind::Reserved {
                        kind: ReservedKind::Acpi,
                        name: multiboot_reserved_name(MemoryAreaType::from(region.typ())),
                    }
                }
                MemoryAreaType::Defective => BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: "defective",
                },
                other => BootMemoryKind::Reserved {
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

fn for_each_linuxboot_region<F>(params_addr: usize, mut f: F)
where
    F: FnMut(BootMemoryKind, usize, usize),
{
    let Some(params) = LinuxBootParams::new(params_addr) else {
        return;
    };

    for index in 0..params.e820_entries() {
        let entry = params
            .e820_entry(index)
            .expect("linux boot params e820 entry out of range");
        let kind = match entry.entry_type {
            x if x == X86LinuxE820EntryType::Ram as u32 => BootMemoryKind::Ram,
            x if x == X86LinuxE820EntryType::Acpi as u32
                || x == X86LinuxE820EntryType::Nvs as u32 =>
            {
                BootMemoryKind::Reserved {
                    kind: ReservedKind::Acpi,
                    name: linuxboot_reserved_name(entry.entry_type),
                }
            }
            x if x == X86LinuxE820EntryType::Persistent as u32 => BootMemoryKind::Reserved {
                kind: ReservedKind::Persistent,
                name: linuxboot_reserved_name(entry.entry_type),
            },
            x if x == X86LinuxE820EntryType::Unusable as u32
                || x == X86LinuxE820EntryType::Disabled as u32 =>
            {
                BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: linuxboot_reserved_name(entry.entry_type),
                }
            }
            x if x == X86LinuxE820EntryType::SoftReserved as u32 => BootMemoryKind::Reserved {
                kind: ReservedKind::Platform,
                name: linuxboot_reserved_name(entry.entry_type),
            },
            _ => BootMemoryKind::Reserved {
                kind: ReservedKind::Firmware,
                name: linuxboot_reserved_name(entry.entry_type),
            },
        };
        f(kind, entry.addr as usize, entry.size as usize);
    }
}

fn for_each_uefi_region<F>(multiboot_info_ptr: usize, mut f: F)
where
    F: FnMut(BootMemoryKind, usize, usize),
{
    if multiboot_info_ptr == 0 {
        return;
    }

    let info = unsafe { ptr::read_unaligned(multiboot_info_ptr as *const UefiMbInfo) };
    if (info.flags & (1 << 6)) == 0 || info.mmap_addr == 0 || info.mmap_length == 0 {
        return;
    }

    let mut cursor = info.mmap_addr as usize;
    let end = cursor + info.mmap_length as usize;
    while cursor + mem::size_of::<UefiMbMmapEntry>() <= end {
        let entry = unsafe { ptr::read_unaligned(cursor as *const UefiMbMmapEntry) };
        if entry.len != 0 {
            let kind = match entry.typ {
                7 => BootMemoryKind::Ram,
                9 | 10 => BootMemoryKind::Reserved {
                    kind: ReservedKind::Acpi,
                    name: uefi_reserved_name(entry.typ),
                },
                14 => BootMemoryKind::Reserved {
                    kind: ReservedKind::Persistent,
                    name: uefi_reserved_name(entry.typ),
                },
                8 | 15 => BootMemoryKind::Reserved {
                    kind: ReservedKind::Unusable,
                    name: uefi_reserved_name(entry.typ),
                },
                _ => BootMemoryKind::Reserved {
                    kind: ReservedKind::Firmware,
                    name: uefi_reserved_name(entry.typ),
                },
            };
            f(kind, entry.addr as usize, entry.len as usize);
        }

        let entry_size = entry.size as usize + mem::size_of::<u32>();
        if entry_size == 0 {
            break;
        }
        cursor += entry_size;
    }
}

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

#[repr(C)]
#[derive(Clone, Copy)]
struct UefiMbMmapEntry {
    size: u32,
    addr: u64,
    len: u64,
    typ: u32,
}

const fn multiboot_reserved_name(kind: MemoryAreaType) -> &'static str {
    match kind {
        MemoryAreaType::Available => "available",
        MemoryAreaType::Reserved => "multiboot reserved",
        MemoryAreaType::AcpiAvailable => "multiboot acpi reclaimable",
        MemoryAreaType::ReservedHibernate => "multiboot hibernate",
        MemoryAreaType::Defective => "multiboot defective",
        _ => "multiboot firmware reserved",
    }
}

const fn linuxboot_reserved_name(entry_type: u32) -> &'static str {
    match entry_type {
        x if x == X86LinuxE820EntryType::Reserved as u32 => "e820 reserved",
        x if x == X86LinuxE820EntryType::Acpi as u32 => "e820 acpi reclaimable",
        x if x == X86LinuxE820EntryType::Nvs as u32 => "e820 acpi nvs",
        x if x == X86LinuxE820EntryType::Unusable as u32 => "e820 unusable",
        x if x == X86LinuxE820EntryType::Disabled as u32 => "e820 disabled",
        x if x == X86LinuxE820EntryType::Persistent as u32 => "e820 persistent",
        x if x == X86LinuxE820EntryType::SoftReserved as u32 => "e820 soft reserved",
        _ => "e820 firmware reserved",
    }
}

const fn uefi_reserved_name(entry_type: u32) -> &'static str {
    match entry_type {
        0 => "uefi reserved",
        1 => "uefi loader code",
        2 => "uefi loader data",
        3 => "uefi boot services code",
        4 => "uefi boot services data",
        5 => "uefi runtime services code",
        6 => "uefi runtime services data",
        8 => "uefi unusable",
        9 => "uefi acpi reclaimable",
        10 => "uefi acpi nvs",
        11 => "mmio",
        12 => "uefi mmio port space",
        13 => "uefi pal code",
        14 => "uefi persistent",
        15 => "uefi unaccepted",
        _ => "uefi firmware reserved",
    }
}
