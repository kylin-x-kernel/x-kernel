// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMU/page table operations for AArch64.

use aarch64_cpu::registers::{Readable, TTBR0_EL1, TTBR1_EL1, Writeable};
use memaddr::PhysAddr;

/// Hardware-ready page-table root value.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HwPageTableRoot(usize);

impl HwPageTableRoot {
    #[inline]
    pub const fn new(raw: usize) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<PhysAddr> for HwPageTableRoot {
    fn from(root_paddr: PhysAddr) -> Self {
        HwPageTableRoot::new(root_paddr.as_usize())
    }
}

/// Reads the current page table root register for kernel space.
///
/// When the `arm-el2` feature is enabled, reads `TTBR0_EL2`; otherwise
/// reads `TTBR1_EL1`.
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_kernel_page_table() -> HwPageTableRoot {
    let pt_root_reg: usize;

    #[cfg(not(feature = "arm-el2"))]
    {
        pt_root_reg = TTBR1_EL1.get() as usize;
    }

    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::TTBR0_EL2;
        pt_root_reg = TTBR0_EL2.get() as usize;
    }

    HwPageTableRoot::new(pt_root_reg)
}

/// Reads the current page table root register for user space (`TTBR0_EL1`).
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_user_page_table() -> HwPageTableRoot {
    let val = TTBR0_EL1.get();
    HwPageTableRoot::new(val as usize)
}

/// Writes the register to update the current page table root for kernel space.
///
/// When the `arm-el2` feature is enabled, writes `TTBR0_EL2`; otherwise
/// writes `TTBR1_EL1`.
///
/// An ISB is issued after the write to synchronise the context change, so
/// that subsequent instructions (including TLBI) operate under the new
/// translation regime.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root: HwPageTableRoot) {
    #[cfg(not(feature = "arm-el2"))]
    {
        TTBR1_EL1.set(root.as_usize() as _);
    }

    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::TTBR0_EL2;
        TTBR0_EL2.set(root.as_usize() as _);
    }

    // ISB synchronises the TTBR write so that subsequent instructions
    // (including TLBI) see the new translation regime.  Without this,
    // a following TLBI may apply to the old TTBR.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

/// Writes the register to update the current page table root for user space
/// (`TTBR0_EL1`).
///
/// An ISB is issued after the write to synchronise the context change, so
/// that subsequent instructions (including TLBI) operate under the new
/// translation regime.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root: HwPageTableRoot) {
    TTBR0_EL1.set(root.as_usize() as _);
    // ISB synchronises the TTBR write so that subsequent instructions
    // (including TLBI) see the new translation regime.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}
