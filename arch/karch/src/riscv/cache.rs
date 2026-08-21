// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for RISC-V.
//!
//! RISC-V `fence.i` synchronises instruction and data memories only on the
//! current hart. For SMP correctness, we use `kiface` to delegate
//! remote hart flushes to the IPI subsystem (`kipi`).

use memaddr::VirtAddr;

/// Local `fence.i` — does not affect other harts.
#[inline]
pub fn flush_icache_all_local() {
    riscv::asm::fence_i();
}

/// What a remote hart must execute when receiving an icache-flush IPI.
///
/// On RISC-V this is the same as [`flush_icache_all_local`] — `fence.i`
/// does not broadcast, so each hart must execute it locally.
#[inline]
pub fn flush_icache_remote() {
    riscv::asm::fence_i();
}

/// Flushes the entire instruction cache on all harts.
///
/// Executes `fence.i` locally, then requests other harts to do the same
/// via IPI (delegated through [`IcacheFlushIf`]).
#[inline]
pub fn flush_icache_all() {
    flush_icache_all_local();
    if kcpu_id_map::nr_cpus() > 1 {
        crate::flush_icache_others();
    }
}

/// Flushes the instruction cache for the given virtual address range on all harts.
///
/// RISC-V does not provide address-range icache operations; delegates to
/// [`flush_icache_all`].
#[inline]
pub fn flush_icache_range(_start: VirtAddr, _size: usize) {
    flush_icache_all();
}

/// Orders prior CPU stores before a device reads the same memory via DMA.
///
/// The RISC-V platforms in this configuration are cache-coherent for device
/// DMA, so no barrier is required. The explicit no-op keeps driver call
/// sites portable across architectures — the LoongArch64 implementation
/// executes `dbar 0`.
#[inline]
pub fn dma_read_barrier() {}
