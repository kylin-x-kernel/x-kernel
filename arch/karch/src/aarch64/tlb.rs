// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TLB maintenance operations for AArch64.

use core::arch::asm;

use memaddr::VirtAddr;

use super::asid::TTBR_ASID_SHIFT;

/// Flushes the TLB entry for `vaddr` tagged with the given user ASID.
#[inline]
pub fn flush_tlb_va_asid(vaddr: VirtAddr, asid: u16) {
    const VA_MASK: u64 = (1 << 44) - 1; // VA[55:12] => bits[43:0]
    let operand = ((asid as u64) << TTBR_ASID_SHIFT) | ((vaddr.as_usize() >> 12) as u64 & VA_MASK);

    #[cfg(not(feature = "arm-el2"))]
    // SAFETY: Valid ASID/VA operand for the TLBI instruction.
    unsafe {
        // TLB Invalidate by VA, ASID, EL1, Inner Shareable
        asm!("tlbi vale1is, {}; dsb sy; isb", in(reg) operand)
    }
    #[cfg(feature = "arm-el2")]
    // SAFETY: Valid VA operand for the TLBI instruction.
    unsafe {
        // No EL1 ASID when running at EL2; fall back to VA-only flush.
        asm!("tlbi vae2is, {}; dsb sy; isb", in(reg) operand & VA_MASK)
    }
}

/// Flushes all TLB entries tagged with the given user ASID.
#[inline]
pub fn flush_tlb_asid(asid: u16) {
    let operand = (asid as u64) << TTBR_ASID_SHIFT;

    #[cfg(not(feature = "arm-el2"))]
    // SAFETY: This is a valid ASID operand for the TLBI instruction.
    unsafe {
        // TLB Invalidate by ASID, EL1, Inner Shareable
        asm!("tlbi aside1is, {}; dsb ish; isb", in(reg) operand)
    }
    #[cfg(feature = "arm-el2")]
    // SAFETY: This is a valid ASID operand for the TLBI instruction.
    unsafe {
        // No EL1 ASID when running at EL2; fall back to full flush.
        asm!("tlbi alle2is; dsb ish; isb")
    }
}

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    if let Some(vaddr) = vaddr {
        const VA_MASK: usize = (1 << 44) - 1; // VA[55:12] => bits[43:0]
        let operand = (vaddr.as_usize() >> 12) & VA_MASK;

        #[cfg(not(feature = "arm-el2"))]
        unsafe {
            // TLB Invalidate by VA, All ASID, EL1, Inner Shareable
            asm!("tlbi vaae1is, {}; dsb sy; isb", in(reg) operand)
        }
        #[cfg(feature = "arm-el2")]
        unsafe {
            // TLB Invalidate by VA, EL2, Inner Shareable
            asm!("tlbi vae2is, {}; dsb sy; isb", in(reg) operand)
        }
    } else {
        // flush the entire TLB
        #[cfg(not(feature = "arm-el2"))]
        unsafe {
            // TLB Invalidate by VMID, All at stage 1, EL1, Inner Shareable
            asm!("dsb ishst; tlbi vmalle1is; dsb ish; isb")
        }
        #[cfg(feature = "arm-el2")]
        unsafe {
            // TLB Invalidate All, EL2, Inner Shareable
            asm!("tlbi alle2is; dsb ish; isb")
        }
    }
}
