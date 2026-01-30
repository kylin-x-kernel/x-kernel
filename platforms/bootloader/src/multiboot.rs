// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{mem, ptr};

use log::info;
use uefi::{
    mem::memory_map::{MemoryDescriptor, MemoryType},
    prelude::Status,
};

pub(crate) fn build_multiboot_info<'a>(
    mbi_buf: u64,
    mmap_iter: impl Iterator<Item = &'a MemoryDescriptor>,
) -> Result<u64, Status> {
    let base = mbi_buf as *mut u8;
    let total_size = 4usize * 0x1000;
    unsafe { ptr::write_bytes(base, 0, total_size) };

    info!(
        "building multiboot info at {:#x}, size={}",
        mbi_buf, total_size
    );

    let mut cursor = unsafe { base.add(mem::size_of::<MbInfo>()) };

    let mut mmap_length = 0u32;
    let mut mem_lower_kb = 0u32;
    let mut mem_upper_kb = 0u32;

    for desc in mmap_iter {
        let start = desc.phys_start;
        let len = desc.page_count * 4096;
        let end = start + len;
        let mtype = if is_available_memory(desc.ty) {
            1u32
        } else {
            2u32
        };
        info!(
            "mmap: type={:?} avail={} start={:#x} pages={} len={:#x}",
            desc.ty,
            mtype == 1,
            start,
            desc.page_count,
            len
        );

        let entry = MbMmapEntry {
            size: (mem::size_of::<MbMmapEntry>() - 4) as u32,
            addr: start,
            len,
            typ: mtype,
        };
        unsafe {
            ptr::write_unaligned(cursor as *mut MbMmapEntry, entry);
            cursor = cursor.add(mem::size_of::<MbMmapEntry>());
        }
        mmap_length += mem::size_of::<MbMmapEntry>() as u32;

        if mtype == 1 {
            if end <= 0x100000 {
                mem_lower_kb += (len / 1024) as u32;
            } else if start >= 0x100000 {
                mem_upper_kb += (len / 1024) as u32;
            } else {
                let low = 0x100000 - start;
                let high = end - 0x100000;
                mem_lower_kb += (low / 1024) as u32;
                mem_upper_kb += (high / 1024) as u32;
            }
        }
    }

    let info = MbInfo {
        flags: (1 << 0) | (1 << 6),
        mem_lower: mem_lower_kb,
        mem_upper: mem_upper_kb,
        boot_device: 0,
        cmdline: 0,
        mods_count: 0,
        mods_addr: 0,
        syms: [0; 4],
        mmap_length,
        mmap_addr: (base as u32).wrapping_add(mem::size_of::<MbInfo>() as u32),
        drives_length: 0,
        drives_addr: 0,
        config_table: 0,
        boot_loader_name: 0,
        apm_table: 0,
        vbe_control_info: 0,
        vbe_mode_info: 0,
        vbe_mode: 0,
        vbe_interface_seg: 0,
        vbe_interface_off: 0,
        vbe_interface_len: 0,
    };

    unsafe {
        ptr::write_unaligned(base as *mut MbInfo, info);
    }

    Ok(base as u64)
}

fn is_available_memory(ty: MemoryType) -> bool {
    matches!(
        ty,
        MemoryType::CONVENTIONAL
            | MemoryType::LOADER_CODE
            | MemoryType::LOADER_DATA
            | MemoryType::BOOT_SERVICES_CODE
            | MemoryType::BOOT_SERVICES_DATA
    )
}

#[repr(C)]
struct MbInfo {
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
struct MbMmapEntry {
    size: u32,
    addr: u64,
    len: u64,
    typ: u32,
}
