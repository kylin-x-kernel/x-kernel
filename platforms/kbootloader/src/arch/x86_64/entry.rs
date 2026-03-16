// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 Multiboot boot entry for kbootloader.
//!
//! Boot flow
//! ---------
//! ```text
//! _start  (.text.boot, 32-bit multiboot entry)
//!   └─ bsp_entry32  – set GDT, CR4, page tables, EFER, CR0, jump to 64-bit
//!        └─ bsp_entry64
//!             └─ rust_entry(magic, mbi)
//!                  └─ PRIMARY_KERNEL_ENTRY(cpu_id, mbi)
//!
//! ap_entry32  (.text.boot, AP 32-bit entry from ap_start.S trampoline)
//!   └─ ap_entry64
//!        └─ rust_entry_secondary(magic)
//!             └─ SECOND_KERNEL_ENTRY(cpu_id)
//! ```

use core::arch::global_asm;

use kbuild_config::{BOOT_STACK_SIZE, PHYS_VIRT_OFFSET, SEV_CBIT_POS};
use x86_64::registers::{
    control::{Cr0Flags, Cr4Flags},
    model_specific::EferFlags,
};

const MULTIBOOT_HEADER_FLAGS: usize = 0x0001_0002;
const MULTIBOOT_HEADER_MAGIC: usize = 0x1BADB002;
pub const MULTIBOOT_BOOTLOADER_MAGIC: usize = 0x2BADB002;

const CR0: u64 = Cr0Flags::PROTECTED_MODE_ENABLE.bits()
    | Cr0Flags::MONITOR_COPROCESSOR.bits()
    | Cr0Flags::NUMERIC_ERROR.bits()
    | Cr0Flags::WRITE_PROTECT.bits()
    | Cr0Flags::PAGING.bits();

const CR4: u64 = Cr4Flags::PHYSICAL_ADDRESS_EXTENSION.bits()
    | Cr4Flags::PAGE_GLOBAL.bits()
    | if cfg!(feature = "fp-simd") {
        Cr4Flags::OSFXSR.bits() | Cr4Flags::OSXMMEXCPT_ENABLE.bits()
    } else {
        0
    };

const EFER: u64 = EferFlags::LONG_MODE_ENABLE.bits() | EferFlags::NO_EXECUTE_ENABLE.bits();

/// AMD SEV / CSV C-bit mask for page table entries.
/// Set to `1 << SEV_CBIT_POS` when SEV/CSV is active; zero otherwise.
/// Use `SEV_CBIT_POS=0` in defconfig to disable (qemu-virt).
pub const SEV_CBIT_MASK: u64 = if SEV_CBIT_POS == 0 {
    0
} else {
    1u64 << SEV_CBIT_POS
};

/// Page index of the AP real-mode startup page (physical address = index × 4 KiB).
///
/// Both x86_64 platforms use page 6 (0x6000). Exported so `mp.rs` can derive
/// the physical address and the SIPI vector from a single source of truth.
pub const AP_START_PAGE_IDX: u8 = 6;
pub const AP_START_PAGE_PADDR: usize = AP_START_PAGE_IDX as usize * 0x1000;

/// Boot stack for the primary CPU.
#[unsafe(link_section = ".bss.stack")]
static mut BOOT_STACK: [u8; BOOT_STACK_SIZE] = [0; BOOT_STACK_SIZE];

global_asm!(
    include_str!("ap_start.S"),
    start_page_paddr = const AP_START_PAGE_PADDR,
);

global_asm!(
    include_str!("multiboot.S"),
    mb_magic        = const MULTIBOOT_BOOTLOADER_MAGIC,
    mb_hdr_magic    = const MULTIBOOT_HEADER_MAGIC,
    mb_hdr_flags    = const MULTIBOOT_HEADER_FLAGS,
    entry           = sym rust_entry,
    entry_secondary = sym rust_entry_secondary,
    offset          = const PHYS_VIRT_OFFSET,
    boot_stack_size = const BOOT_STACK_SIZE,
    boot_stack      = sym BOOT_STACK,
    cr0             = const CR0,
    cr4             = const CR4,
    efer_msr        = const x86::msr::IA32_EFER,
    efer            = const EFER,
    cbit_mask       = const SEV_CBIT_MASK,
);

/// Read the initial APIC ID from CPUID leaf 1 (bits [31:24] of EBX).
/// Used as the logical CPU ID, matching the convention of both x86 platforms.
fn get_cpu_id() -> usize {
    // rbx is reserved by LLVM; save/restore it around the CPUID instruction.
    let ebx_val: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {:e}, ebx",
            "pop rbx",
            out(reg) ebx_val,
            inout("eax") 1u32 => _,
            out("ecx") _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    ((ebx_val >> 24) & 0xff) as usize
}

/// Primary CPU C entry: validates multiboot magic, then dispatches via
/// [`PRIMARY_KERNEL_ENTRY`](crate::PRIMARY_KERNEL_ENTRY).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_entry(magic: usize, mbi: usize) {
    if magic == MULTIBOOT_BOOTLOADER_MAGIC {
        call_kernel_entry!(PRIMARY_KERNEL_ENTRY, get_cpu_id(), mbi)
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) }
    }
}

/// Secondary CPU C entry: dispatches via
/// [`SECOND_KERNEL_ENTRY`](crate::SECOND_KERNEL_ENTRY).
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_entry_secondary(_magic: usize) {
    if _magic == MULTIBOOT_BOOTLOADER_MAGIC {
        call_kernel_entry!(SECOND_KERNEL_ENTRY, get_cpu_id())
    }
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) }
    }
}
