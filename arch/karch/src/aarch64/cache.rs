// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for AArch64.
//!
//! AArch64 `ic ivau` / `ic iallu` broadcast to the inner shareable domain via
//! hardware, so icache entries are invalidated on all PEs. However, only an
//! `isb` on a given PE flushes that PE's pipeline of stale prefetched
//! instructions. We therefore send an IPI (via `crate_interface` → `kipi`) so
//! every PE executes `isb`.

use core::arch::asm;

use memaddr::VirtAddr;

/// Local icache flush and pipeline sync — does not affect other PEs.
#[inline]
pub fn flush_icache_all_local() {
    unsafe { asm!("ic iallu; dsb sy; isb") };
}

/// What a remote PE must execute when receiving an icache-flush IPI.
///
/// The icache invalidation already broadcast to all PEs via hardware (`ic ivau`
/// / `ic iallu`). The remote PE only needs an `isb` to flush stale instructions
/// from its pipeline. Called from the `kipi` IPI callback.
#[inline]
pub fn flush_icache_remote() {
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

/// Flushes the entire instruction cache and synchronises pipelines on all PEs.
///
/// Executes `ic iallu; dsb sy; isb` locally, then sends IPIs so other PEs
/// execute `isb` to flush their pipelines.
#[inline]
pub fn flush_icache_all() {
    flush_icache_all_local();
    if kbuild_config::CPU_NUM > 1 {
        crate::flush_icache_others();
    }
}

/// Flushes the instruction cache for the given virtual address range on all PEs.
///
/// Cleans data cache and invalidates instruction cache per cache line for
/// `[start, start + size)`; the `ic ivau` broadcast invalidates the icache on
/// all PEs. An IPI is then sent so other PEs execute `isb`.
#[inline]
pub fn flush_icache_range(start: VirtAddr, size: usize) {
    const CACHE_LINE: usize = 64;
    let addr = start.as_usize() & !(CACHE_LINE - 1);
    let end = (start.as_usize() + size + CACHE_LINE - 1) & !(CACHE_LINE - 1);
    for va in (addr..end).step_by(CACHE_LINE) {
        // SAFETY: `dc cvau` cleans by VA to PoU,
        // matching Linux arm64 caches_clean_inval_pou() semantics. `ic ivau`
        // invalidates I-cache by VA to PoU. Both instructions are standard
        // ARM instructions, safe at all privilege levels.
        unsafe {
            asm!("dc cvau, {0:x}", in(reg) va);
            asm!("ic ivau, {0:x}", in(reg) va);
        }
    }
    aarch64_cpu::asm::barrier::dsb(aarch64_cpu::asm::barrier::ISH);
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);

    if kbuild_config::CPU_NUM > 1 {
        crate::flush_icache_others();
    }
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
