// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{mem, ptr};

use boot_info::{BootInfo, BootProtocol, LinuxBootParams};
use multiboot2::{BootInformation, BootInformationHeader, MemoryAreaType};

pub fn for_each_ram_region<F>(boot_info: &BootInfo, f: F)
where
    F: FnMut(usize, usize),
{
    match boot_info.protocol() {
        BootProtocol::Multiboot2 => for_each_multiboot_region(boot_info.protocol_info_addr, f),
        BootProtocol::Uefi => for_each_uefi_region(boot_info.protocol_info_addr, f),
        BootProtocol::LinuxBoot => for_each_linuxboot_region(boot_info.protocol_info_addr, f),
        _ => {}
    }
}

fn for_each_multiboot_region<F>(multiboot_info_ptr: usize, mut f: F)
where
    F: FnMut(usize, usize),
{
    if multiboot_info_ptr == 0 {
        return;
    }
    let info = unsafe { BootInformation::load(multiboot_info_ptr as *const BootInformationHeader) }
        .expect("invalid multiboot2 boot information");
    if let Some(mmap) = info.memory_map_tag() {
        for region in mmap.memory_areas() {
            if MemoryAreaType::from(region.typ()) == MemoryAreaType::Available {
                f(region.start_address() as usize, region.size() as usize);
            }
        }
    }
}

fn for_each_linuxboot_region<F>(params_addr: usize, mut f: F)
where
    F: FnMut(usize, usize),
{
    let Some(params) = LinuxBootParams::new(params_addr) else {
        return;
    };

    for index in 0..params.e820_entries() {
        let entry = params
            .e820_entry(index)
            .expect("linux boot params e820 entry out of range");
        if entry.is_usable_ram() {
            f(entry.addr as usize, entry.size as usize);
        }
    }
}

fn for_each_uefi_region<F>(multiboot_info_ptr: usize, mut f: F)
where
    F: FnMut(usize, usize),
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
        if entry.typ == 1 && entry.len != 0 {
            f(entry.addr as usize, entry.len as usize);
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
