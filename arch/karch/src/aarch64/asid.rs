// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 TTBR0_EL1 ASID packing helpers.

use memaddr::PhysAddr;

use super::mmu::HwPageTableRoot;

/// ASID bit width when TCR_EL1.AS is set to 16-bit ASID mode.
pub const USER_ASID_BITS: u32 = 16;

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
