// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::memory::{PageAligned, pa};

use crate::config::plat::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET};
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];
#[unsafe(link_section = ".data")]
static mut BOOT_PT_SV39: PageAligned<[u64; 512]> = PageAligned::new([0; 512]);
#[allow(clippy::identity_op)]
unsafe fn init_boot_page_table() {
    unsafe {
        BOOT_PT_SV39[0] = (0x0 << 10) | 0xef;
        BOOT_PT_SV39[2] = (0x80000 << 10) | 0xef;
        BOOT_PT_SV39[0x100] = (0x0 << 10) | 0xef;
        BOOT_PT_SV39[0x102] = (0x80000 << 10) | 0xef;
    }
}
unsafe fn init_mmu() {
    unsafe {
        kcpu::instrs::write_kernel_page_table(pa!(&raw const BOOT_PT_SV39 as usize));
        kcpu::instrs::flush_tlb(None);
    }
}
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!("
        mv      s0, a0                  // save hartid
        mv      s1, a1                  // save DTB pointer
        la      sp, {boot_stack}
        li      t0, {boot_stack_size}
        add     sp, sp, t0              // setup boot stack
        call    {init_boot_page_table}
        call    {init_mmu}              // setup boot page table and enabel MMU
        li      s2, {phys_virt_offset}  // fix up virtual high address
        add     sp, sp, s2
        mv      a0, s0
        mv      a1, s1
        la      a2, {entry}
        add     a2, a2, s2
        jalr    a2                      // call_main(cpu_id, dtb)
        j       .",
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        boot_stack_size = const BOOT_STACK_SIZE,
        boot_stack = sym BOOT_STACK,
        init_boot_page_table = sym init_boot_page_table,
        init_mmu = sym init_mmu,
        entry = sym kplat::entry,
    )
}
#[cfg(feature = "smp")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn _start_secondary() -> ! {
    core::arch::naked_asm!("
        mv      s0, a0                  // save hartid
        mv      sp, a1                  // set SP
        call    {init_mmu}              // setup boot page table and enabel MMU
        li      s1, {phys_virt_offset}  // fix up virtual high address
        add     a1, a1, s1
        add     sp, sp, s1
        mv      a0, s0
        la      a1, {entry}
        add     a1, a1, s1
        jalr    a1                      // call_secondary_main(cpu_id)
        j       .",
        phys_virt_offset = const PHYS_VIRT_OFFSET,
        init_mmu = sym init_mmu,
        entry = sym kplat::entry_secondary,
    )
}
