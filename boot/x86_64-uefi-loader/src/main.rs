// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
#![cfg(target_arch = "x86_64")]

extern crate alloc;

use core::ptr;

use boot_info::{BOOT_INFO_MAGIC, BootInfo, BootProtocol, HardwareDescriptionRoot};
use log::{error, info};
use uefi::{
    boot::AllocateType,
    mem::memory_map::{MemoryMap, MemoryType},
    prelude::*,
    system,
    table::cfg::ConfigTableEntry,
};
use x86_boot_common::{
    BootPagingError, PageAllocator, build_boot_info, build_page_tables, jump_to_kernel,
    switch_page_table,
};

mod config;
mod loader;
mod multiboot;

#[entry]
fn efi_main() -> Status {
    if let Err(err) = uefi::helpers::init() {
        let _ = err;
    }

    info!("bootloader: start");

    let image_handle = uefi::boot::image_handle();
    info!("image handle = {:?}", image_handle);

    let cfg = config::load_config(image_handle);

    let kernel = match loader::load_kernel(image_handle, &cfg.kernel_paths) {
        Ok(data) => data,
        Err(status) => {
            error!("load_kernel failed: {:?}", status);
            return status;
        }
    };

    let loaded_kernel = match loader::load_kernel_image(&kernel) {
        Ok(v) => v,
        Err(status) => {
            error!("load_kernel_image failed: {:?}", status);
            return status;
        }
    };

    info!(
        "kernel entry: pa={:#x}, va={:#x}, load_pa={:#x}, image={:#x}..{:#x}",
        loaded_kernel.entry_paddr,
        loaded_kernel.entry_vaddr,
        loaded_kernel.load_paddr,
        loaded_kernel.image_vaddr_range.0,
        loaded_kernel.image_vaddr_range.1
    );

    let stack_top = loaded_kernel.boot_stack_top_vaddr;
    info!("stack top = {:#x}", stack_top);

    let (pml4, cbit_mask) =
        match build_kernel_page_tables(loaded_kernel.load_paddr, loaded_kernel.image_vaddr_range) {
            Ok(v) => v,
            Err(status) => {
                error!("build_page_tables failed: {:?}", status);
                return status;
            }
        };
    info!("page tables: pml4={:#x}, cbit_mask={:#x}", pml4, cbit_mask);

    let rsdp_addr = find_rsdp_addr();
    if rsdp_addr != 0 {
        info!("acpi rsdp = {:#x}", rsdp_addr);
    }

    let multiboot_info_buf = match alloc_low_pages(4) {
        Ok(v) => v,
        Err(status) => {
            error!("alloc_low_pages(multiboot) failed: {:?}", status);
            return status;
        }
    };
    info!("multiboot info buffer = {:#x}", multiboot_info_buf);

    let boot_info_buf = match alloc_low_pages(1) {
        Ok(v) => v,
        Err(status) => {
            error!("alloc_low_pages(boot_info) failed: {:?}", status);
            return status;
        }
    };
    info!("boot info buffer = {:#x}", boot_info_buf);

    info!("exiting boot services...");
    let mmap = unsafe { uefi::boot::exit_boot_services(None) };

    let protocol_info_addr =
        match multiboot::build_multiboot_info(multiboot_info_buf, mmap.entries()) {
            Ok(addr) => addr as usize,
            Err(status) => {
                error!("build_multiboot_info failed: {:?}", status);
                return status;
            }
        };

    unsafe {
        ptr::write(
            boot_info_buf as *mut BootInfo,
            build_boot_info(
                BootProtocol::Uefi,
                protocol_info_addr,
                loaded_kernel.load_paddr as usize,
                0.into(),
                kbuild_config::CPU_NUM,
            )
            .with_hardware_description_root(HardwareDescriptionRoot::Acpi)
            .with_rsdp(rsdp_addr),
        );
    }

    info!("jumping to kernel stub...");

    unsafe {
        switch_page_table(pml4, cbit_mask);
        jump_to_kernel(
            loaded_kernel.entry_vaddr,
            BOOT_INFO_MAGIC,
            boot_info_buf,
            stack_top,
        );
    }

    #[allow(unreachable_code)]
    Status::SUCCESS
}

fn build_kernel_page_tables(
    kernel_load_paddr: u64,
    kernel_vaddr_range: (u64, u64),
) -> Result<(u64, u64), Status> {
    let cbit_mask = sev_cbit_mask();
    info!("sev cbit mask = {:#x}", cbit_mask);
    let mut allocator = UefiPageAllocator;
    let pml4 = match build_page_tables(
        &mut allocator,
        kernel_load_paddr,
        kernel_vaddr_range,
        cbit_mask,
    ) {
        Ok(pml4) => pml4,
        Err(BootPagingError::Alloc(status)) => return Err(status),
        Err(BootPagingError::KernelSpansMultiplePdptEntries { start, end }) => {
            error!(
                "kernel image spans multiple PDPT entries: {:#x}..{:#x}",
                start, end
            );
            return Err(Status::UNSUPPORTED);
        }
    };
    Ok((pml4, cbit_mask))
}

struct UefiPageAllocator;

impl PageAllocator for UefiPageAllocator {
    type Error = Status;

    fn alloc_page(&mut self) -> Result<u64, Self::Error> {
        alloc_page()
    }
}

fn alloc_page() -> Result<u64, Status> {
    let paddr = uefi::boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .map_err(|e| e.status())?
        .as_ptr() as u64;
    info!("alloc_page: paddr={:#x}", paddr);
    Ok(paddr)
}

fn alloc_low_pages(pages: usize) -> Result<u64, Status> {
    info!("alloc_low_pages: pages={}", pages);
    let paddr = uefi::boot::allocate_pages(
        AllocateType::MaxAddress(0xffff_ffff),
        MemoryType::LOADER_DATA,
        pages,
    )
    .map_err(|e| e.status())?
    .as_ptr() as u64;
    info!("alloc_low_pages: paddr={:#x}", paddr);
    Ok(paddr)
}

pub(crate) fn pages_for(size: u64) -> usize {
    ((size + 0xfff) / 0x1000) as usize
}

fn find_rsdp_addr() -> usize {
    system::with_config_table(|tables| {
        tables
            .iter()
            .find(|entry| entry.guid == ConfigTableEntry::ACPI2_GUID)
            .or_else(|| {
                tables
                    .iter()
                    .find(|entry| entry.guid == ConfigTableEntry::ACPI_GUID)
            })
            .map(|entry| entry.address as usize)
            .unwrap_or(0)
    })
}

fn sev_cbit_mask() -> u64 {
    let max = cpuid(0x8000_0000, 0).0;
    info!("cpuid max extended leaf = {:#x}", max);
    if max < 0x8000_001f {
        return 0;
    }
    let (eax, ebx, ..) = cpuid(0x8000_001f, 0);
    info!("cpuid 0x8000_001f: eax={:#x} ebx={:#x}", eax, ebx);
    if (eax & (1 << 1)) == 0 {
        return 0;
    }
    let cbit_pos = (ebx & 0x3f) as u64;
    info!("sev cbit position = {}", cbit_pos);
    if cbit_pos == 0 { 0 } else { 1u64 << cbit_pos }
}

#[cfg(target_arch = "x86_64")]
fn cpuid(eax: u32, ecx: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid_count(eax, ecx);
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[cfg(target_arch = "x86")]
fn cpuid(eax: u32, ecx: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86::__cpuid_count(eax, ecx);
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
fn cpuid(_eax: u32, _ecx: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}
