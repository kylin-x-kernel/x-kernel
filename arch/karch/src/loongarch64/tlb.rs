// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TLB maintenance operations for LoongArch64.

use core::arch::asm;

use memaddr::VirtAddr;

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    // SAFETY: these instructions operate only on the current CPU's TLB state;
    // the optional virtual address is passed verbatim to the architected `invtlb`.
    unsafe {
        if let Some(vaddr) = vaddr {
            // <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#_dbar>
            //
            // Only after all previous load/store access operations are completely
            // executed, the DBAR 0 instruction can be executed; and only after the
            // execution of DBAR 0 is completed, all subsequent load/store access
            // operations can be executed.
            //
            // <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#_invtlb>
            //
            // formats: invtlb op, asid, addr
            //
            // op 0x5: Clear all page table entries with G=0 and ASID equal to the
            // register specified ASID, and VA equal to the register specified VA.
            //
            // When the operation indicated by op does not require an ASID, the
            // general register rj should be set to r0.
            asm!("dbar 0; invtlb 0x05, $r0, {reg}", reg = in(reg) vaddr.as_usize());
        } else {
            // op 0x0: Clear all page table entries
            asm!("dbar 0; invtlb 0x00, $r0, $r0");
        }
    }
}
