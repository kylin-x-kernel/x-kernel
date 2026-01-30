#![no_std]
#![no_main]

extern crate alloc;

use core::{ptr, slice};

use log::{error, info};
use uefi::{
    boot::AllocateType,
    mem::memory_map::{MemoryMap, MemoryType},
    prelude::*,
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

    let (entry_vaddr, kernel_range) =
        match loader::load_kernel_image(&kernel, cfg.kernel_base_paddr, cfg.phys_virt_offset) {
            Ok(v) => v,
            Err(status) => {
                error!("load_kernel_image failed: {:?}", status);
                return status;
            }
        };

    info!(
        "kernel entry = {:#x}, image = {:#x}..{:#x}",
        entry_vaddr, kernel_range.0, kernel_range.1
    );

    let stack_top = match alloc_stack(64 * 1024, cfg.phys_virt_offset) {
        Ok(v) => v,
        Err(status) => {
            error!("alloc_stack failed: {:?}", status);
            return status;
        }
    };
    info!("stack top = {:#x}", stack_top);

    let (pml4, cbit_mask) = match build_page_tables() {
        Ok(v) => v,
        Err(status) => {
            error!("build_page_tables failed: {:?}", status);
            return status;
        }
    };
    info!("page tables: pml4={:#x}, cbit_mask={:#x}", pml4, cbit_mask);

    let mbi_buf = match alloc_low_pages(4) {
        Ok(v) => v,
        Err(status) => {
            error!("alloc_low_pages failed: {:?}", status);
            return status;
        }
    };
    info!("multiboot info buffer = {:#x}", mbi_buf);

    info!("exiting boot services...");
    let mmap = unsafe { uefi::boot::exit_boot_services(None) };

    let mbi_ptr = match multiboot::build_multiboot_info(mbi_buf, mmap.entries()) {
        Ok(v) => v,
        Err(status) => {
            error!("build_multiboot_info failed: {:?}", status);
            return status;
        }
    };

    info!("jumping to kernel (bsp_entry64)...");

    unsafe {
        switch_page_table(pml4, cbit_mask);
        jump_to_kernel(entry_vaddr, cfg.multiboot_magic, mbi_ptr as u64, stack_top);
    }

    #[allow(unreachable_code)]
    Status::SUCCESS
}

fn build_page_tables() -> Result<(u64, u64), Status> {
    let cbit_mask = sev_cbit_mask();
    info!("sev cbit mask = {:#x}", cbit_mask);

    let pml4 = alloc_page()?;
    let pdpt_low = alloc_page()?;
    let pdpt_high = alloc_page()?;
    info!(
        "page tables allocated: pml4={:#x} pdpt_low={:#x} pdpt_high={:#x}",
        pml4, pdpt_low, pdpt_high
    );

    unsafe {
        ptr::write_bytes(pml4 as *mut u8, 0, 0x1000);
        ptr::write_bytes(pdpt_low as *mut u8, 0, 0x1000);
        ptr::write_bytes(pdpt_high as *mut u8, 0, 0x1000);

        let pml4_entries = slice::from_raw_parts_mut(pml4 as *mut u64, 512);
        let pdpt_low_entries = slice::from_raw_parts_mut(pdpt_low as *mut u64, 512);
        let pdpt_high_entries = slice::from_raw_parts_mut(pdpt_high as *mut u64, 512);

        let flags = 0x3u64;
        let ps_flags = 0x83u64;

        pml4_entries[0] = (pdpt_low | cbit_mask) | flags;
        pml4_entries[256] = (pdpt_high | cbit_mask) | flags;

        for i in 0..512u64 {
            let paddr = i << 30;
            let entry = (paddr | cbit_mask) | ps_flags;
            pdpt_low_entries[i as usize] = entry;
            pdpt_high_entries[i as usize] = entry;
        }
    }

    Ok((pml4, cbit_mask))
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

fn alloc_stack(size: usize, phys_virt_offset: u64) -> Result<u64, Status> {
    let pages = pages_for(size as u64);
    info!("alloc_stack: size={} pages={}", size, pages);
    let paddr = uefi::boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
        .map_err(|e| e.status())?
        .as_ptr() as u64;
    let vaddr = paddr + phys_virt_offset;
    Ok(vaddr + size as u64)
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
    // SAFETY: CPUID is available on x86_64.
    let r = unsafe { core::arch::x86_64::__cpuid_count(eax, ecx) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[cfg(target_arch = "x86")]
fn cpuid(eax: u32, ecx: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID is available on x86.
    let r = core::arch::x86::__cpuid_count(eax, ecx);
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
fn cpuid(_eax: u32, _ecx: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}

unsafe fn switch_page_table(pml4: u64, cbit_mask: u64) {
    let cr3 = pml4 | cbit_mask;
    unsafe {
        core::arch::asm!(
            "mov cr3, {0}",
            in(reg) cr3,
            options(nostack, preserves_flags)
        );
    }
}

unsafe fn jump_to_kernel(entry: u64, magic: u64, mbi: u64, stack_top: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "cli",
            "mov rsp, {0}",
            "mov rdi, {1}",
            "mov rsi, {2}",
            "jmp {3}",
            in(reg) stack_top,
            in(reg) magic,
            in(reg) mbi,
            in(reg) entry,
            options(noreturn)
        );
    }
}
