// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};

mod aarch64;
mod fallback;
mod loongarch64;
mod riscv64;
mod x86_64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutConsts {
    pub pg_va_bits: usize,
    pub kernel_aspace_base: usize,
    pub kernel_aspace_size: usize,
    pub linear_map_vaddr: usize,
    pub linear_map_vsize: usize,
    pub page_offset: usize,
    pub iomap_vaddr: usize,
    pub iomap_vsize: usize,
    pub kimage_vaddr: usize,
    pub kimage_vsize: usize,
}

/// User-space virtual memory layout constants (per-architecture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserLayoutConsts {
    pub user_space_base: usize,
    pub user_space_size: usize,
    pub user_interp_base: usize,
    pub user_heap_base: usize,
    pub user_heap_size: usize,
    pub user_heap_size_max: usize,
    pub signal_trampoline: usize,
    pub user_stack_top: usize,
    pub user_stack_size: usize,
}

fn arch_layouts(arch: &str) -> (LayoutConsts, UserLayoutConsts) {
    match arch {
        "aarch64" => (aarch64::LAYOUT, aarch64::USER_LAYOUT),
        "riscv64" => (riscv64::LAYOUT, riscv64::USER_LAYOUT),
        "x86_64" => (x86_64::LAYOUT, x86_64::USER_LAYOUT),
        "riscv32" => (fallback::LAYOUT, fallback::USER_LAYOUT),
        "loongarch64" => (loongarch64::LAYOUT, loongarch64::USER_LAYOUT),
        _ => (fallback::LAYOUT, fallback::USER_LAYOUT),
    }
}

cfg_select! {
    target_arch = "aarch64" => {
        use self::aarch64 as current_arch_layout;
    }
    target_arch = "x86_64" => {
        use self::x86_64 as current_arch_layout;
    }
    target_arch = "riscv64" => {
        use self::riscv64 as current_arch_layout;
    }
    target_arch = "loongarch64" => {
        use self::loongarch64 as current_arch_layout;
    }
    target_arch = "riscv32" => {
        use self::fallback as current_arch_layout;
    }
    _ => {
        use self::fallback as current_arch_layout;
    }
}

pub fn for_arch(arch: &str) -> LayoutConsts {
    arch_layouts(arch).0
}

pub fn user_layout_for_arch(arch: &str) -> UserLayoutConsts {
    arch_layouts(arch).1
}

const CURRENT_LAYOUT: LayoutConsts = current_arch_layout::LAYOUT;
const CURRENT_USER_LAYOUT: UserLayoutConsts = current_arch_layout::USER_LAYOUT;

pub const PG_VA_BITS: usize = CURRENT_LAYOUT.pg_va_bits;
pub const KERNEL_ASPACE_BASE: usize = CURRENT_LAYOUT.kernel_aspace_base;
pub const KERNEL_ASPACE_SIZE: usize = CURRENT_LAYOUT.kernel_aspace_size;
pub const LINEAR_MAP_VADDR: usize = CURRENT_LAYOUT.linear_map_vaddr;
pub const LINEAR_MAP_VSIZE: usize = CURRENT_LAYOUT.linear_map_vsize;
pub const PAGE_OFFSET: usize = CURRENT_LAYOUT.page_offset;
pub const IOMAP_VADDR: usize = CURRENT_LAYOUT.iomap_vaddr;
pub const IOMAP_VSIZE: usize = CURRENT_LAYOUT.iomap_vsize;
pub const KIMAGE_VADDR: usize = CURRENT_LAYOUT.kimage_vaddr;
pub const KIMAGE_VSIZE: usize = CURRENT_LAYOUT.kimage_vsize;
pub const BOOT_IO_VADDR: usize = IOMAP_VADDR;
pub const BOOT_IO_VSIZE: usize = IOMAP_VSIZE;

// User-space layout constants.
pub const USER_SPACE_BASE: usize = CURRENT_USER_LAYOUT.user_space_base;
pub const USER_SPACE_SIZE: usize = CURRENT_USER_LAYOUT.user_space_size;
pub const USER_INTERP_BASE: usize = CURRENT_USER_LAYOUT.user_interp_base;
pub const USER_HEAP_BASE: usize = CURRENT_USER_LAYOUT.user_heap_base;
pub const USER_HEAP_SIZE: usize = CURRENT_USER_LAYOUT.user_heap_size;
pub const USER_HEAP_SIZE_MAX: usize = CURRENT_USER_LAYOUT.user_heap_size_max;
pub const SIGNAL_TRAMPOLINE: usize = CURRENT_USER_LAYOUT.signal_trampoline;
pub const USER_STACK_TOP: usize = CURRENT_USER_LAYOUT.user_stack_top;
pub const USER_STACK_SIZE: usize = CURRENT_USER_LAYOUT.user_stack_size;

/// Boot-only MMIO slots are sized to a 2 MiB block so the early page-table
/// layout can reserve devices on a coarse, architecture-friendly boundary
/// without encoding full physical addresses into virtual addresses.
pub const BOOT_IO_SLOT_SIZE: usize = 0x20_0000;
pub const BOOT_UART_SLOT: usize = 0;
pub const BOOT_UART_SLOT_VADDR: usize = BOOT_IO_VADDR + BOOT_UART_SLOT * BOOT_IO_SLOT_SIZE;

const fn is_power_of_two(value: usize) -> bool {
    value != 0 && (value & (value - 1)) == 0
}

const _: () = assert!(is_power_of_two(BOOT_IO_SLOT_SIZE));
const _: () = assert!(BOOT_IO_VSIZE == 0 || BOOT_IO_SLOT_SIZE <= BOOT_IO_VSIZE);
const _: () = assert!(BOOT_IO_VSIZE == 0 || BOOT_UART_SLOT < (BOOT_IO_VSIZE / BOOT_IO_SLOT_SIZE));

// Compile-time invariants for user-space layout constants.
const _: () = assert!(USER_SPACE_SIZE > 0);
const _: () = assert!(USER_STACK_SIZE > 0);
const _: () = assert!(USER_HEAP_SIZE > 0);
const _: () = assert!(USER_HEAP_SIZE_MAX >= USER_HEAP_SIZE);
const _: () = assert!(USER_STACK_TOP > USER_STACK_SIZE);
const _: () = assert!(SIGNAL_TRAMPOLINE > USER_SPACE_BASE);
const _: () = assert!(USER_HEAP_BASE >= USER_SPACE_BASE);
const _: () = assert!(USER_HEAP_BASE + USER_HEAP_SIZE <= USER_SPACE_BASE + USER_SPACE_SIZE);

/// Runtime offset from physical kernel load address to the linked kernel-image
/// virtual address.
///
/// `kimage_voffset = KIMAGE_VADDR - PA(_start)`
///
/// This is established by the bootloader before entering the relocated kernel
/// and then used by common address-conversion helpers.
static KIMAGE_VOFFSET: AtomicUsize = AtomicUsize::new(0);

/// Records the kernel-image VA-to-PA offset established during early boot.
#[inline]
pub fn set_kimage_voffset(offset: usize) {
    let current = KIMAGE_VOFFSET.load(Ordering::Relaxed);
    if current != 0 && current != offset {
        panic!("kimage voffset changed unexpectedly: old={current:#x}, new={offset:#x}");
    }
    KIMAGE_VOFFSET.store(offset, Ordering::Relaxed);
}

/// Returns the kernel-image VA-to-PA offset established during early boot.
#[inline]
pub fn kimage_voffset() -> usize {
    KIMAGE_VOFFSET.load(Ordering::Relaxed)
}

/// Convert a physical address to its linear-map virtual address.
#[inline]
pub fn p2v(pa: usize) -> usize {
    pa + PAGE_OFFSET
}

#[inline]
const fn in_window(va: usize, start: usize, size: usize) -> bool {
    va >= start && (va - start) < size
}

#[inline]
pub(crate) fn v2p_with_kimage_window(va: usize, layout: LayoutConsts) -> usize {
    if in_window(va, layout.kimage_vaddr, layout.kimage_vsize) {
        va - kimage_voffset()
    } else if in_window(va, layout.linear_map_vaddr, layout.linear_map_vsize) {
        va - layout.page_offset
    } else {
        panic!("v2p only supports linear-map or kernel-image addresses: {va:#x}");
    }
}

cfg_select! {
    any(
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "x86_64",
        target_arch = "riscv64"
    ) => {
        /// Convert a virtual address to its physical address.
        #[inline]
        pub fn v2p(va: usize) -> usize {
            v2p_with_kimage_window(va, CURRENT_LAYOUT)
        }
    }
    _ => {
        compile_error!("`v2p` is only supported on the current 64-bit kernel architectures");
    }
}
