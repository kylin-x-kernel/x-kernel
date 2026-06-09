// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User ASID allocation for AArch64 TTBR0_EL1.

use core::sync::atomic::{AtomicU32, Ordering};

use memaddr::PhysAddr;

use super::{flush_tlb, flush_tlb_asid, mmu::HwPageTableRoot};

/// ASID bit width when TCR_EL1.AS == 0 (default 16-bit ASID).
pub const USER_ASID_BITS: u32 = 16;

const USER_ASID_MAX: u32 = (1 << USER_ASID_BITS) - 1;

/// ASID 0 is reserved for the boot/idmap translation regime.
const USER_ASID_MIN: u32 = 1;

static NEXT_USER_ASID: AtomicU32 = AtomicU32::new(USER_ASID_MIN);

/// Shift for packing ASID into TTBR0_EL1[63:48].
pub const TTBR_ASID_SHIFT: u32 = 48;

/// Physical address mask for TTBR0_EL1 (bits [47:0] with 4 KiB alignment).
const TTBR_BADDR_MASK: u64 = (1 << TTBR_ASID_SHIFT as u64) - 1;

/// Encodes a user page-table root and ASID into a hardware TTBR0_EL1 value.
#[inline]
pub const fn encode_user_page_table_root(root_paddr: PhysAddr, asid: u16) -> HwPageTableRoot {
    let baddr = root_paddr.as_usize() as u64 & TTBR_BADDR_MASK;
    HwPageTableRoot::new((baddr | ((asid as u64) << TTBR_ASID_SHIFT)) as usize)
}

/// Returns the physical page-table root encoded in a TTBR0_EL1 value.
#[inline]
pub fn user_page_table_root_paddr(ttbr: HwPageTableRoot) -> PhysAddr {
    PhysAddr::from((ttbr.as_usize() as u64 & TTBR_BADDR_MASK) as usize)
}

/// Returns the ASID encoded in a TTBR0_EL1 value.
#[inline]
pub const fn user_asid_from_ttbr(ttbr: HwPageTableRoot) -> u16 {
    (ttbr.as_usize() >> TTBR_ASID_SHIFT) as u16
}

/// Allocates a fresh user ASID.
///
/// On ASID-space exhaustion the allocator rolls over, flushes the entire TLB,
/// and reuses ASIDs from 1.
pub fn alloc_user_asid() -> u16 {
    loop {
        let asid = NEXT_USER_ASID.fetch_add(1, Ordering::Relaxed);
        if asid <= USER_ASID_MAX {
            return asid as u16;
        }
        NEXT_USER_ASID.store(USER_ASID_MIN, Ordering::Relaxed);
        flush_tlb(None);
    }
}

/// Releases a user ASID, invalidating any remaining TLB entries tagged with it.
pub fn free_user_asid(asid: u16) {
    if asid != 0 {
        flush_tlb_asid(asid);
    }
}
