// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for AArch64.

use core::arch::asm;

use memaddr::VirtAddr;

/// Flushes the entire instruction cache.
#[inline]
pub fn flush_icache_all() {
    unsafe { asm!("ic iallu; dsb sy; isb") };
}

/// Flushes the data cache line at the given virtual address.
///
/// Uses the `DC IVAC` instruction (Data Cache Invalidate by Virtual Address to
/// Point of Coherency). The cache line size is implementation-defined; 64 bytes
/// is typical for AArch64 but may vary across CPU implementations.
#[inline]
pub fn flush_dcache_line(vaddr: VirtAddr) {
    unsafe { asm!("dc ivac, {0:x}; dsb sy; isb", in(reg) vaddr.as_usize()) };
}
