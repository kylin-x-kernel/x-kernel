// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V 64 position-independent boot entry.

use core::arch::naked_asm;

use boot_info::{BootInfo, BootProtocol, HardwareDescriptionRoot, MemoryDescriptionRoot};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::{BOOT_STACK_SIZE, CPU_NUM};
#[cfg(feature = "fp-simd")]
use riscv::register::sstatus::{self, FS};

use super::mmu;

/// Boot stack for the primary CPU.
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

/// Unified boot info passed from the RISC-V boot entry into the runtime.
static mut RISCV_BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::OpenSBI);

#[unsafe(link_section = ".text.boot")]
fn enable_fp_simd() {
    #[cfg(feature = "fp-simd")]
    unsafe {
        sstatus::set_fs(FS::Initial);
    }
}

/// Primary CPU early boot entry.
///
/// On entry:
/// - `a0` = hart id
/// - `a1` = device-tree physical address
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "mv      s0, a0",
        "mv      s1, a1",
        "la      sp, {boot_stack}",
        "li      t0, {boot_stack_size}",
        "add     sp, sp, t0",
        "mv      a0, s1",
        "call    {create_boot_page_tables}",
        "call    {enable_fp_simd}",
        "call    {init_mmu}",
        "la      s2, {kernel_start}",
        "li      t0, {kimage_vaddr}",
        "sub     s2, t0, s2",
        "add     sp, sp, s2",
        "mv      a0, s0",
        "mv      a1, s1",
        "mv      a2, s2",
        "la      t0, {primary_switched}",
        "add     t0, t0, s2",
        "jalr    t0",
        "j       .",
        boot_stack = sym BOOT_STACK,
        boot_stack_size = const BOOT_STACK_SIZE,
        create_boot_page_tables = sym mmu::create_boot_page_tables,
        enable_fp_simd = sym enable_fp_simd,
        init_mmu = sym mmu::init_mmu,
        kernel_start = sym _start,
        kimage_vaddr = const KIMAGE_VADDR,
        primary_switched = sym __primary_switched,
    )
}

/// Secondary CPU early boot entry.
///
/// # Safety
/// On entry:
/// - `a0` = hart id
/// - `a1` = top of a pre-allocated physical boot stack
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.boot")]
pub unsafe extern "C" fn _start_secondary() -> ! {
    naked_asm!(
        "mv      s0, a0",
        "mv      sp, a1",
        "call    {enable_fp_simd}",
        "call    {init_mmu}",
        "la      s1, {kernel_start}",
        "li      t0, {kimage_vaddr}",
        "sub     s1, t0, s1",
        "add     sp, sp, s1",
        "mv      a0, s0",
        "la      t0, {entry_secondary}",
        "add     t0, t0, s1",
        "jalr    t0",
        "j       .",
        enable_fp_simd = sym enable_fp_simd,
        init_mmu = sym mmu::init_mmu,
        kernel_start = sym _start,
        kimage_vaddr = const KIMAGE_VADDR,
        entry_secondary = sym __secondary_switched,
    )
}

pub unsafe extern "C" fn __secondary_switched(cpu_id: usize) -> ! {
    call_kernel_entry!(SECOND_KERNEL_ENTRY, cpu_id);
    loop {
        core::hint::spin_loop();
    }
}

/// Post-MMU entry point for the boot CPU.
pub unsafe extern "C" fn __primary_switched(
    cpu_id: usize,
    dtb_paddr: usize,
    kimage_voffset: usize,
) -> ! {
    unsafe extern "C" {
        fn _sbss();
        fn _ebss();
    }

    unsafe {
        let bss_start = _sbss as *const () as usize;
        let bss_end = _ebss as *const () as usize;
        core::slice::from_raw_parts_mut(bss_start as *mut u8, bss_end - bss_start).fill(0);
    }

    kaddr_layout::set_kimage_voffset(kimage_voffset);
    // Keep the linear-map console handoff in Rust after BSS is cleared and the
    // runtime kimage offset is established. Do not add boot-console logging in
    // the init_mmu() -> __primary_switched() window unless this activation is
    // moved earlier again.
    super::serial::activate_linear_map();

    let kernel_load_paddr = KIMAGE_VADDR - kimage_voffset;
    unsafe {
        RISCV_BOOT_INFO = BootInfo::new(BootProtocol::OpenSBI)
            .with_memory_description_root(MemoryDescriptionRoot::DeviceTree)
            .with_hardware_description_root(HardwareDescriptionRoot::DeviceTree)
            .with_protocol_info_addr(dtb_paddr)
            .with_kernel_load_paddr(kernel_load_paddr)
            .with_phys_virt_offset(PAGE_OFFSET)
            .with_dtb(dtb_paddr, kaddr_layout::p2v(dtb_paddr))
            .with_boot_console_mmio(
                kbuild_config::BOOT_CONSOLE_ADDR,
                0x1000,
                PAGE_OFFSET + kbuild_config::BOOT_CONSOLE_ADDR,
            )
            .with_cpu_id(cpu_id)
            .with_cpu_count(CPU_NUM);
    }

    let boot_info_ptr = core::ptr::addr_of!(RISCV_BOOT_INFO) as usize;
    call_kernel_entry!(PRIMARY_KERNEL_ENTRY, boot_info_ptr);
    loop {
        core::hint::spin_loop();
    }
}
