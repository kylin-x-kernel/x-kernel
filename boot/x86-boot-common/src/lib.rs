// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::ptr;

use boot_info::{BootInfo, BootProtocol};
use kaddr_layout::PAGE_OFFSET;
use kernel_elf_loader::KernelElf;

pub const ALIGN_2M: u64 = 0x20_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadedKernel {
    pub entry_paddr: u64,
    pub entry_vaddr: u64,
    pub boot_stack_top_vaddr: u64,
    pub load_paddr: u64,
    pub image_vaddr_range: (u64, u64),
}

pub fn kernel_image_size(image: &[u8]) -> Result<u64, &'static str> {
    Ok(KernelElf::parse(image)?.image_size())
}

pub fn load_kernel_elf(image: &[u8], load_paddr: u64) -> Result<LoadedKernel, &'static str> {
    let elf = KernelElf::parse(image)?;
    let loaded = LoadedKernel {
        entry_paddr: elf
            .paddr_for_vaddr(load_paddr, elf.entry_point_vaddr())
            .ok_or("invalid kernel entry point")?,
        entry_vaddr: elf
            .find_symbol_value("rust_entry")
            .ok_or("missing rust_entry")?,
        boot_stack_top_vaddr: elf
            .find_symbol_value("boot_stack_top")
            .ok_or("missing boot_stack_top")?,
        load_paddr,
        image_vaddr_range: elf.image_vaddr_range(),
    };
    unsafe { elf.load_to(load_paddr)? };
    Ok(loaded)
}

pub fn build_boot_info(
    protocol: BootProtocol,
    protocol_info_addr: usize,
    kernel_load_paddr: usize,
    cpu_id: usize,
    cpu_count: usize,
) -> BootInfo {
    BootInfo::new(protocol)
        .with_protocol_info_addr(protocol_info_addr)
        .with_kernel_load_paddr(kernel_load_paddr)
        .with_phys_virt_offset(PAGE_OFFSET)
        .with_cpu_id(cpu_id)
        .with_cpu_count(cpu_count)
}

pub trait PageAllocator {
    type Error;

    fn alloc_page(&mut self) -> Result<u64, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootPagingError<E> {
    Alloc(E),
    KernelSpansMultiplePdptEntries { start: u64, end: u64 },
}

pub fn build_page_tables<A: PageAllocator>(
    allocator: &mut A,
    kernel_load_paddr: u64,
    kernel_vaddr_range: (u64, u64),
    cbit_mask: u64,
) -> Result<u64, BootPagingError<A::Error>> {
    let pml4 = allocator.alloc_page().map_err(BootPagingError::Alloc)?;
    let pdpt_low = allocator.alloc_page().map_err(BootPagingError::Alloc)?;
    let pdpt_high = allocator.alloc_page().map_err(BootPagingError::Alloc)?;
    let pdpt_kimage = allocator.alloc_page().map_err(BootPagingError::Alloc)?;
    let pd_kimage = allocator.alloc_page().map_err(BootPagingError::Alloc)?;

    unsafe {
        ptr::write_bytes(pml4 as *mut u8, 0, 0x1000);
        ptr::write_bytes(pdpt_low as *mut u8, 0, 0x1000);
        ptr::write_bytes(pdpt_high as *mut u8, 0, 0x1000);
        ptr::write_bytes(pdpt_kimage as *mut u8, 0, 0x1000);
        ptr::write_bytes(pd_kimage as *mut u8, 0, 0x1000);

        let pml4_entries = ptr::slice_from_raw_parts_mut(pml4 as *mut u64, 512);
        let pdpt_low_entries = ptr::slice_from_raw_parts_mut(pdpt_low as *mut u64, 512);
        let pdpt_high_entries = ptr::slice_from_raw_parts_mut(pdpt_high as *mut u64, 512);
        let pdpt_kimage_entries = ptr::slice_from_raw_parts_mut(pdpt_kimage as *mut u64, 512);
        let pd_kimage_entries = ptr::slice_from_raw_parts_mut(pd_kimage as *mut u64, 512);

        let flags = 0x3u64;
        let ps_flags = 0x83u64;

        (*pml4_entries)[0] = (pdpt_low | cbit_mask) | flags;
        (*pml4_entries)[256] = (pdpt_high | cbit_mask) | flags;
        (*pml4_entries)[511] = (pdpt_kimage | cbit_mask) | flags;

        for i in 0..512u64 {
            let entry = ((i << 30) | cbit_mask) | ps_flags;
            (*pdpt_low_entries)[i as usize] = entry;
            (*pdpt_high_entries)[i as usize] = entry;
        }

        let kernel_vstart = kernel_vaddr_range.0 & !0xfff;
        let kernel_vend = align_up(kernel_vaddr_range.1, 0x1000);
        let pdpt_idx = ((kernel_vstart >> 30) & 0x1ff) as usize;
        let end_pdpt_idx = (((kernel_vend - 1) >> 30) & 0x1ff) as usize;
        if pdpt_idx != end_pdpt_idx {
            return Err(BootPagingError::KernelSpansMultiplePdptEntries {
                start: kernel_vstart,
                end: kernel_vend,
            });
        }

        (*pdpt_kimage_entries)[pdpt_idx] = (pd_kimage | cbit_mask) | flags;

        let start_pd_idx = ((kernel_vstart >> 21) & 0x1ff) as usize;
        let end_pd_idx = (((kernel_vend - 1) >> 21) & 0x1ff) as usize;
        for pd_idx in start_pd_idx..=end_pd_idx {
            let pt = allocator.alloc_page().map_err(BootPagingError::Alloc)?;
            ptr::write_bytes(pt as *mut u8, 0, 0x1000);
            (*pd_kimage_entries)[pd_idx] = (pt | cbit_mask) | flags;

            let pt_entries = ptr::slice_from_raw_parts_mut(pt as *mut u64, 512);
            let pt_vaddr_base =
                (kernel_vstart & !0x1f_ffff) + ((pd_idx - start_pd_idx) as u64) * 0x20_0000;
            let page_base = kernel_load_paddr + (pt_vaddr_base - kernel_vstart);
            for i in 0..512u64 {
                let vaddr = pt_vaddr_base + i * 0x1000;
                if vaddr < kernel_vstart || vaddr >= kernel_vend {
                    continue;
                }
                (*pt_entries)[i as usize] = ((page_base + i * 0x1000) | cbit_mask) | flags;
            }
        }
    }

    Ok(pml4)
}

#[inline]
pub fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

pub fn overlapping_reserved_end(
    candidate_start: u64,
    candidate_end: u64,
    reserved: &[(u64, u64)],
) -> Option<u64> {
    reserved
        .iter()
        .filter_map(|&(start, end)| {
            if candidate_start < end && start < candidate_end {
                Some(end)
            } else {
                None
            }
        })
        .max()
}

/// Switches to the temporary/final x86_64 boot page table.
///
/// # Safety
///
/// `pml4` must point to a valid, fully initialized x86_64 top-level page table
/// that is safe to activate on the current CPU. The caller must also ensure the
/// current execution path, stack, and data needed after the `cr3` write remain
/// mapped under the new address space. `cbit_mask` must match the encryption bit
/// policy used when the page table entries were created.
pub unsafe fn switch_page_table(pml4: u64, cbit_mask: u64) {
    let cr3 = pml4 | cbit_mask;
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }
}

/// Transfers control to the loaded kernel entry point.
///
/// # Safety
///
/// `entry_vaddr` must be a valid executable kernel entry under the currently
/// active page table. `stack_top` must point to writable mapped stack memory,
/// and `arg`/`magic` must satisfy the kernel entry contract expected by that
/// entry point. This function never returns.
pub unsafe fn jump_to_kernel(entry_vaddr: u64, magic: u64, arg: u64, stack_top: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "cli",
            "mov rsp, {0}",
            "mov rdi, {1}",
            "mov rsi, {2}",
            "jmp {3}",
            in(reg) stack_top,
            in(reg) magic,
            in(reg) arg,
            in(reg) entry_vaddr,
            options(noreturn)
        );
    }
}
