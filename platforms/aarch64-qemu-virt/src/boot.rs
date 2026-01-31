// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot entry and early page table setup for aarch64-qemu-virt.

use kplat::memory::{PageAligned, pa};
use page_table::{
    PageTableEntry as GenericPTE, PagingFlags as MappingFlags, aarch64::A64PageEntry as A64PTE,
};

use crate::config::plat::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET};
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L0: PageAligned<[A64PTE; 512]> = PageAligned::new([A64PTE::empty(); 512]);
#[unsafe(link_section = ".data")]
static mut BOOT_PT_L1: PageAligned<[A64PTE; 512]> = PageAligned::new([A64PTE::empty(); 512]);
unsafe fn init_boot_page_table() {
    unsafe {
        BOOT_PT_L0[0] = A64PTE::new_table(pa!(&raw mut BOOT_PT_L1 as usize));
        BOOT_PT_L1[0] = A64PTE::new_page(
            pa!(0),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
            true,
        );
        BOOT_PT_L1[1] = A64PTE::new_page(
            pa!(0x4000_0000),
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE,
            true,
        );
    }
}
unsafe fn enable_fp() {
    #[cfg(feature = "fp-simd")]
    kcpu::instrs::enable_fp();
}
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    const FLAG_LE: usize = 0b0;
    const FLAG_PAGE_SIZE_4K: usize = 0b10;
    const FLAG_ANY_MEM: usize = 0b1000;
    core::arch::naked_asm!("
        add     x13, x18, #0x16     // 'MZ' magic
        b       {entry}             // Branch to kernel start, magic
        .quad   0                   // Image load offset from start of RAM, little-endian
        .quad   _ekernel - _start   // Effective size of kernel image, little-endian
        .quad   {flags}             // Kernel flags, little-endian
        .quad   0                   // reserved
        .quad   0                   // reserved
        .quad   0                   // reserved
        .ascii  \"ARM\\x64\"        // Magic number
        .long   0                   // reserved (used for PE COFF offset)",
        flags = const FLAG_LE | FLAG_PAGE_SIZE_4K | FLAG_ANY_MEM,
        entry = sym _start_primary,
    )
}
#[unsafe(naked)]
unsafe extern "C" fn _start_primary() -> ! {
    core::arch::naked_asm!("
        mrs     x19, mpidr_el1
        and     x19, x19, #0xffffff     // get current CPU id
        mov     x20, x0                 // save DTB pointer
        adrp    x8, {boot_stack}        // setup boot stack
        add     x8, x8, {boot_stack_size}
        mov     sp, x8
        bl      {switch_to_el1}         // switch to EL1
        bl      {enable_fp}             // enable fp/neon
        bl      {init_boot_page_table}
        adrp    x0, {boot_pt}
        bl      {init_mmu}              // setup MMU
        mov     x8, {phys_virt_offset}  // set SP to the high address
        add     sp, sp, x8
        mov     x0, x19                 // call_main(cpu_id, dtb)
        mov     x1, x20
        ldr     x8, ={entry}
        blr     x8
        b      .",
        switch_to_el1 = sym kcpu::boot::switch_to_el1,
        init_mmu = sym kcpu::boot::init_mmu,
        init_boot_page_table = sym init_boot_page_table,
        enable_fp = sym enable_fp,
        boot_pt = sym BOOT_PT_L0,
        boot_stack = sym BOOT_STACK,
        boot_stack_size = const BOOT_STACK_SIZE,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym kplat::entry,
    )
}
#[cfg(feature = "smp")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    core::arch::naked_asm!("
        mrs     x19, mpidr_el1
        and     x19, x19, #0xffffff     // get current CPU id
        mov     sp, x0
        bl      {switch_to_el1}
        bl      {enable_fp}
        adrp    x0, {boot_pt}
        bl      {init_mmu}
        mov     x8, {phys_virt_offset}  // set SP to the high address
        add     sp, sp, x8
        mov     x0, x19                 // call_secondary_main(cpu_id)
        ldr     x8, ={entry}
        blr     x8
        b      .",
        switch_to_el1 = sym kcpu::boot::switch_to_el1,
        init_mmu = sym kcpu::boot::init_mmu,
        enable_fp = sym enable_fp,
        boot_pt = sym BOOT_PT_L0,
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        entry = sym kplat::entry_secondary,
    )
}
