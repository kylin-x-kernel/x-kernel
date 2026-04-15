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

pub fn for_arch(arch: &str) -> LayoutConsts {
    match arch {
        "aarch64" => aarch64::LAYOUT,
        "riscv64" => riscv64::LAYOUT,
        "x86_64" => x86_64::LAYOUT,
        "riscv32" => fallback::LAYOUT,
        "loongarch64" => loongarch64::LAYOUT,
        _ => fallback::LAYOUT,
    }
}

#[cfg(target_arch = "aarch64")]
const CURRENT_LAYOUT: LayoutConsts = aarch64::LAYOUT;
#[cfg(target_arch = "x86_64")]
const CURRENT_LAYOUT: LayoutConsts = x86_64::LAYOUT;
#[cfg(target_arch = "riscv32")]
const CURRENT_LAYOUT: LayoutConsts = fallback::LAYOUT;
#[cfg(target_arch = "riscv64")]
const CURRENT_LAYOUT: LayoutConsts = riscv64::LAYOUT;
#[cfg(target_arch = "loongarch64")]
const CURRENT_LAYOUT: LayoutConsts = loongarch64::LAYOUT;
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
const CURRENT_LAYOUT: LayoutConsts = fallback::LAYOUT;

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

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
))]
#[inline]
const fn in_window(va: usize, start: usize, size: usize) -> bool {
    va >= start && (va - start) < size
}

/// Convert a virtual address to its physical address.
///
/// AArch64/x86_64 keep the linked kernel image in a dedicated higher-half
/// window distinct from the linear map, so kernel-image VAs must subtract the
/// runtime `kimage_voffset()`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
pub fn v2p(va: usize) -> usize {
    if in_window(va, KIMAGE_VADDR, KIMAGE_VSIZE) {
        va - kimage_voffset()
    } else if in_window(va, LINEAR_MAP_VADDR, LINEAR_MAP_VSIZE) {
        va - PAGE_OFFSET
    } else {
        panic!("v2p only supports linear-map or kernel-image addresses: {va:#x}");
    }
}

/// Convert a virtual address to its physical address.
///
/// RISC-V currently uses a dedicated kernel-image alias window too, so it
/// shares the same split logic as AArch64/x86_64.
#[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
#[inline]
pub fn v2p(va: usize) -> usize {
    if in_window(va, KIMAGE_VADDR, KIMAGE_VSIZE) {
        va - kimage_voffset()
    } else if in_window(va, LINEAR_MAP_VADDR, LINEAR_MAP_VSIZE) {
        va - PAGE_OFFSET
    } else {
        panic!("v2p only supports linear-map or kernel-image addresses: {va:#x}");
    }
}

/// Fallback architectures still translate kernel VAs through the linear-map
/// offset only.
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
)))]
#[inline]
pub fn v2p(va: usize) -> usize {
    va - PAGE_OFFSET
}
