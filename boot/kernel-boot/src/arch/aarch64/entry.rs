// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 position-independent boot entry.
//!
//! Boot flow
//! ---------
//! ```text
//! _start  (.head.text)
//!   └─ primary_entry  (.idmap.text)
//!        ├─ preserve_boot_args()   – save x0-x3 (DTB, …) via adrp
//!        ├─ switch_to_el1()        – EL3/EL2 → EL1 transition
//!        ├─ enable_fp()            – enable FP/SIMD
//!        ├─ create_boot_page_tables() – build idmap + kernel high map
//!        ├─ init_mmu()             – set MAIR/TCR/TTBR, enable MMU
//!        └─ __primary_switched()  (virtual address)
//!             ├─ zero BSS
//!             └─ kplat::entry(cpu_id, boot_info)
//! ```

use core::arch::naked_asm;

use boot_info::{BootInfo, BootProtocol};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::BOOT_STACK_SIZE;

use super::{el, mmu, serial};

// Linux ARM64 Boot Protocol image flags.
const FLAG_LE: usize = 0b0;
const FLAG_PAGE_SIZE_4K: usize = 0b10;
const FLAG_ANY_MEM: usize = 0b1000;

/// Boot stack for the primary CPU.
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

/// Storage for the boot arguments passed in x0-x3 by firmware/bootloader.
#[unsafe(link_section = ".data")]
pub(super) static mut SAVED_BOOT_ARGS: [u64; 4] = [0; 4];

/// Unified boot info passed from the AArch64 boot entry into the kernel.
static mut AARCH64_BOOT_INFO: BootInfo = BootInfo::new(BootProtocol::DeviceTree);

/// Linux ARM64 Boot Protocol header followed by a branch to `primary_entry`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".head.text")]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "add     x13, x18, #0x16",         // "MZ" magic (valid ARM64 no-op instruction)
        "bl      {entry}",                 // branch to kernel start
        ".quad   0",                       // image load offset from RAM base (little-endian)
        ".quad   _ekernel - _start",       // effective image size
        ".quad   {flags}",                 // kernel flags
        ".quad   0",                       // reserved
        ".quad   0",                       // reserved
        ".quad   0",                       // reserved
        ".ascii  \"ARM\\x64\"",            // magic number
        ".long   0",                       // reserved (PE COFF offset)
        flags = const FLAG_LE | FLAG_PAGE_SIZE_4K | FLAG_ANY_MEM,
        entry = sym primary_entry,
    )
}

/// Primary CPU early boot entry (runs before MMU is enabled).
///
/// All code here is position-independent – only PC-relative addressing is
/// used for data, except for `ldr x8, =sym` literal-pool loads which
/// intentionally load the *linked* virtual address so that the `br x8`
/// after MMU-enable jumps to the correct high-virtual-address symbol.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn primary_entry() -> ! {
    naked_asm!(
        // Capture CPU ID from MPIDR_EL1[23:0] (Aff2|Aff1|Aff0) and DTB
        // pointer before any call clobbers them.  This simplified affinity
        // masking follows the same convention used by other x-kernel platforms
        // (e.g. aarch64-qemu-virt) and is sufficient for typical SMP
        // configurations where Aff3 is zero.
        "mrs     x19, mpidr_el1",
        "and     x19, x19, #0xffffff",   // CPU affinity Aff2|Aff1|Aff0
        "mov     x20, x0",               // save DTB physical address

        // Save firmware boot arguments (x0-x3) to SAVED_BOOT_ARGS via adrp.
        "bl      {preserve_boot_args}",

        // Set up the early boot stack using PC-relative addressing.
        "adrp    x8, {boot_stack}",
        "add     x8, x8, :lo12:{boot_stack}",
        "add     x8, x8, {boot_stack_size}",
        "mov     sp, x8",

        // Drop to EL1 (no-op when already at EL1).
        "bl      {switch_to_el1}",

        // Enable FP/SIMD so that Rust code can use float registers.
        "bl      {enable_fp}",

        // Build the two-level boot page tables (idmap + kernel high map).
        "bl      {create_boot_page_tables}",

        // Program MAIR/TCR/TTBR and enable the MMU.
        "bl      {init_mmu}",

        // Switch the stack pointer to its high virtual address at KIMAGE_VADDR.
        // The boot stack lives inside the kernel image, so its virtual address is:
        //   SP_virt = SP_phys + (KIMAGE_VADDR - PA(_start))
        // Compute PA(_start) via adrp and derive the adjustment.
        "adrp    x8, {kernel_start}",          // x8 = PA(_start), page-aligned
        "ldr     x9, ={kimage_vaddr}",          // x9 = KIMAGE_VADDR (compile-time const)
        "sub     x8, x9, x8",                  // x8 = KIMAGE_VADDR - PA(_start) = kimage_voffset
        "add     sp, sp, x8",

        // Restore cpu_id, DTB, and pass kimage_voffset for __primary_switched.
        "mov     x0, x19",
        "mov     x1, x20",
        "mov     x2, x8",                      // x2 = kimage_voffset

        // Jump to the virtual address of __primary_switched.
        // `ldr x8, =sym` loads the *linked* VMA from the literal pool so
        // that the branch targets the high-VA mapping set up above.
        "ldr     x8, ={primary_switched}",
        "blr     x8",
        "b .",

        preserve_boot_args      = sym preserve_boot_args,
        boot_stack              = sym BOOT_STACK,
        boot_stack_size         = const BOOT_STACK_SIZE,
        switch_to_el1           = sym el::switch_to_el1,
        enable_fp               = sym enable_fp,
        create_boot_page_tables = sym mmu::create_boot_page_tables,
        init_mmu                = sym mmu::init_mmu,
        kernel_start            = sym _start,
        kimage_vaddr            = const KIMAGE_VADDR,
        primary_switched        = sym __primary_switched,
    )
}

/// Save x0-x3 (firmware boot arguments) to [`SAVED_BOOT_ARGS`].
///
/// Uses PC-relative addressing so this can run before the MMU is on.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn preserve_boot_args() {
    naked_asm!(
        // Get the physical address of SAVED_BOOT_ARGS via adrp/add.
        "adrp    x8, {saved_args}",
        "add     x8, x8, :lo12:{saved_args}",
        // Store x0..x3.
        "stp     x0, x1, [x8]",
        "stp     x2, x3, [x8, #16]",
        // Full system barrier so the stores complete before the MMU is enabled.
        "dmb     sy",
        "ret",
        saved_args = sym SAVED_BOOT_ARGS,
    )
}

/// Secondary CPU boot entry.
///
/// Called with `x0` = top of a pre-allocated stack.
///
/// # Safety
///
/// Must only be called from secondary CPUs, with `x0` = top of a pre-allocated stack.
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub unsafe extern "C" fn _start_secondary() -> ! {
    naked_asm!(
        "mrs     x19, mpidr_el1",
        "and     x19, x19, #0xffffff",   // CPU affinity Aff2|Aff1|Aff0 (see primary_entry)
        "mov     sp, x0",                // stack passed in x0 (physical address)
        "bl      {switch_to_el1}",
        "bl      {enable_fp}",
        "bl      {init_mmu}",
        // Adjust SP to KIMAGE_VADDR range (same as primary_entry):
        //   kimage_voffset = KIMAGE_VADDR - PA(_start)
        // Secondary boot stacks are in .bss.stack (kernel image, mapped at KIMAGE_VADDR).
        // The runtime page table maps kernel image PAs only at KIMAGE_VADDR, not at
        // PAGE_OFFSET+PA, so we must use kimage_voffset here to keep SP valid
        // after init_memory_management_secondary() switches to the runtime page table.
        "adrp    x8, {kernel_start}",
        "ldr     x9, ={kimage_vaddr}",
        "sub     x8, x9, x8",           // x8 = kimage_voffset
        "add     sp, sp, x8",
        "mov     x0, x19",               // cpu_id
        "ldr     x8, ={entry_secondary}",
        "br      x8",
        switch_to_el1    = sym el::switch_to_el1,
        enable_fp        = sym enable_fp,
        init_mmu         = sym mmu::init_mmu,
        kernel_start     = sym _start,
        kimage_vaddr     = const KIMAGE_VADDR,
        entry_secondary  = sym __secondary_switched,
    )
}

pub unsafe extern "C" fn __secondary_switched(cpu_id: usize) {
    call_kernel_entry!(SECOND_KERNEL_ENTRY, cpu_id)
}

/// Post-MMU entry point – runs at the kernel's high virtual address.
///
/// Receives `kimage_voffset = KIMAGE_VADDR - PA(_start)` computed in
/// `primary_entry` and stores it in `kaddr_layout` for later use by the memory
/// subsystem (v2p / p2v for kernel-image symbols).
///
/// Zeroes BSS, then constructs a [`BootInfo`] payload and calls
/// [`kplat::entry`] with the boot CPU id and boot info pointer.
///
/// # Safety
///
/// Must only be called once, from [`primary_entry`], after the MMU has been
/// enabled and the stack pointer adjusted to a virtual address.
pub unsafe extern "C" fn __primary_switched(
    cpu_id: usize,
    dtb_paddr: usize,
    kimage_voffset: usize,
) {
    // Zero BSS before setting any global state, so the AtomicUsize storing
    // kimage_voffset (which lives in .bss) is cleared first and not
    // overwritten by the fill below.
    unsafe extern "C" {
        fn _sbss();
        fn _ebss();
    }
    unsafe {
        let bss_start = _sbss as *const () as usize;
        let bss_end = _ebss as *const () as usize;
        core::slice::from_raw_parts_mut(bss_start as *mut u8, bss_end - bss_start).fill(0);
    }

    // Store the runtime VA-to-PA offset now that BSS is clean.  All
    // subsequent v2p()/p2v() calls on kernel-image symbols depend on this.
    kaddr_layout::set_kimage_voffset(kimage_voffset);

    let kernel_load_paddr = KIMAGE_VADDR - kimage_voffset;
    unsafe {
        AARCH64_BOOT_INFO = BootInfo::new(BootProtocol::DeviceTree)
            .with_protocol_info_addr(dtb_paddr)
            .with_kernel_load_paddr(kernel_load_paddr)
            .with_phys_virt_offset(PAGE_OFFSET)
            .with_dtb(dtb_paddr)
            .with_boot_console_mmio(
                kbuild_config::BOOT_CONSOLE_ADDR,
                0x1000,
                serial::BOOT_UART_BOOT_VADDR,
            )
            .with_cpu_id(cpu_id)
            .with_cpu_count(kbuild_config::CPU_NUM);
    }
    crate::bootln!(
        "entered primary switched cpu={} dtb={:#x} kimage_voffset={:#x}",
        cpu_id,
        dtb_paddr,
        kimage_voffset
    );
    super::mmu::extend_boot_linear_ram_from_dtb(dtb_paddr);
    crate::bootln!("boot linear RAM map extended from DT");
    let boot_info_ptr = core::ptr::addr_of!(AARCH64_BOOT_INFO) as usize;
    crate::bootln!("handoff to kruntime boot_info={boot_info_ptr:#x}");
    call_kernel_entry!(PRIMARY_KERNEL_ENTRY, boot_info_ptr)
}

/// Enable FP/SIMD by clearing traps in `CPACR_EL1`.
#[unsafe(link_section = ".idmap.text")]
fn enable_fp() {
    #[cfg(feature = "fp-simd")]
    karch::enable_fp();
}
