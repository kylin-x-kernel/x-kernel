// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for LoongArch64.
//!
//! LoongArch64 `ibar 0` invalidates the instruction cache only on the current
//! core. For SMP correctness, we use `crate_interface` to delegate remote
//! core flushes to the IPI subsystem (`kipi`).

use core::arch::asm;

use memaddr::VirtAddr;

/// Local `ibar 0` — does not affect other cores.
#[inline]
pub fn flush_icache_all_local() {
    unsafe { asm!("ibar 0") };
}

/// What a remote core must execute when receiving an icache-flush IPI.
///
/// On LoongArch64 this is the same as [`flush_icache_all_local`] — `ibar 0`
/// does not broadcast, so each core must execute it locally.
#[inline]
pub fn flush_icache_remote() {
    unsafe { asm!("ibar 0") };
}

/// Flushes the entire instruction cache on all cores.
///
/// Executes `ibar 0` locally, then requests other cores to do the same
/// via IPI (delegated through [`IcacheFlushIf`]).
#[inline]
pub fn flush_icache_all() {
    flush_icache_all_local();
    if kbuild_config::CPU_NUM > 1 {
        crate::flush_icache_others();
    }
}

/// Flushes the instruction cache for the given virtual address range on all cores.
///
/// Uses full `ibar 0` flush as provided by the LoongArch ISA; delegates to
/// [`flush_icache_all`].
#[inline]
pub fn flush_icache_range(_start: VirtAddr, _size: usize) {
    flush_icache_all();
}
