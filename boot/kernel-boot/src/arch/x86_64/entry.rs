// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 Multiboot2 assembly stub for kernel_boot.
//!
//! This module only owns the earliest boot path:
//! - Multiboot2 header
//! - 32-bit to 64-bit transition
//! - temporary page tables
//! - runtime `kimage_voffset` detection
//!
//! The handoff into the higher-half kernel lives in `handoff.rs` so the future
//! Linux-style low-address stub can keep the same Rust handoff boundary.

use core::arch::global_asm;

use kbuild_config::{BOOT_STACK_SIZE, SEV_CBIT_POS};
use x86_64::registers::{
    control::{Cr0Flags, Cr4Flags},
    model_specific::EferFlags,
};

use super::handoff::{rust_entry, rust_entry_secondary};

pub const MULTIBOOT_BOOTLOADER_MAGIC: usize = 0x36d7_6289;

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
    entry           = sym rust_entry,
    entry_secondary = sym rust_entry_secondary,
    boot_stack_size = const BOOT_STACK_SIZE,
    boot_stack      = sym BOOT_STACK,
    cr0             = const CR0,
    cr4             = const CR4,
    efer_msr        = const x86::msr::IA32_EFER,
    efer            = const EFER,
    cbit_mask       = const SEV_CBIT_MASK,
);
