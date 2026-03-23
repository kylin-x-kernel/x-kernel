// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutConsts {
    pub pg_va_bits: usize,
    pub page_offset: usize,
    pub kimage_vaddr: usize,
    pub kimage_vsize: usize,
}

const fn lower_half_split_layout() -> LayoutConsts {
    const PG_VA_BITS: usize = 48;
    let page_offset = (-(1i64 << PG_VA_BITS)) as usize;
    let kimage_vsize = (1usize << PG_VA_BITS) / 0x10;
    let modules_vsize = kimage_vsize * 0x8;
    LayoutConsts {
        pg_va_bits: PG_VA_BITS,
        page_offset,
        kimage_vaddr: page_offset + modules_vsize,
        kimage_vsize,
    }
}

const fn x86_64_layout() -> LayoutConsts {
    LayoutConsts {
        pg_va_bits: 48,
        page_offset: 0xffff_8000_0000_0000,
        kimage_vaddr: 0xffff_ff80_0000_0000,
        kimage_vsize: 0x0000_0080_0000_0000,
    }
}

const fn fallback_layout() -> LayoutConsts {
    LayoutConsts {
        pg_va_bits: 48,
        page_offset: 0xffff_0000_0000_0000,
        kimage_vaddr: 0xffff_8000_0000_0000,
        kimage_vsize: 0x0010_0000_0000_0000,
    }
}

pub fn for_arch(arch: &str) -> LayoutConsts {
    match arch {
        "aarch64" | "riscv32" | "riscv64" | "loongarch64" => lower_half_split_layout(),
        "x86_64" => x86_64_layout(),
        _ => fallback_layout(),
    }
}

#[cfg(target_arch = "aarch64")]
const CURRENT_LAYOUT: LayoutConsts = lower_half_split_layout();
#[cfg(target_arch = "x86_64")]
const CURRENT_LAYOUT: LayoutConsts = x86_64_layout();
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
const CURRENT_LAYOUT: LayoutConsts = lower_half_split_layout();
#[cfg(target_arch = "loongarch64")]
const CURRENT_LAYOUT: LayoutConsts = lower_half_split_layout();
#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv32",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
const CURRENT_LAYOUT: LayoutConsts = fallback_layout();

pub const PG_VA_BITS: usize = CURRENT_LAYOUT.pg_va_bits;
pub const PAGE_OFFSET: usize = CURRENT_LAYOUT.page_offset;
pub const KIMAGE_VADDR: usize = CURRENT_LAYOUT.kimage_vaddr;
pub const KIMAGE_VSIZE: usize = CURRENT_LAYOUT.kimage_vsize;

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
    KIMAGE_VOFFSET.store(offset, Ordering::Relaxed);
}

/// Returns the kernel-image VA-to-PA offset established during early boot.
#[inline]
pub fn kimage_voffset() -> usize {
    KIMAGE_VOFFSET.load(Ordering::Relaxed)
}

/// Convert a physical address to its linear-map virtual address.
///
/// `p2v(pa) = pa + PAGE_OFFSET`.
#[inline]
pub fn p2v(pa: usize) -> usize {
    pa + PAGE_OFFSET
}

/// Convert a virtual address to its physical address.
///
/// Architectures with runtime kernel-image relocation keep the linear map and
/// kernel image in different virtual regions, so kernel-image addresses must
/// subtract the runtime `kimage_voffset`.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
pub fn v2p(va: usize) -> usize {
    if va >= KIMAGE_VADDR {
        va - kimage_voffset()
    } else {
        va - PAGE_OFFSET
    }
}

/// Convert a virtual address to its physical address.
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline]
pub fn v2p(va: usize) -> usize {
    va - PAGE_OFFSET
}
