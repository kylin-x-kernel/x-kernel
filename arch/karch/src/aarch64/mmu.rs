// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMU/page table operations for AArch64.

use aarch64_cpu::registers::{Readable, TTBR0_EL1, TTBR1_EL1, Writeable};
use memaddr::PhysAddr;

/// Reads the current page table root register for kernel space.
///
/// When the `arm-el2` feature is enabled, reads `TTBR0_EL2`; otherwise
/// reads `TTBR1_EL1`.
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
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

    PhysAddr::from(pt_root_reg)
}

/// Reads the current page table root register for user space (`TTBR0_EL1`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    let val = TTBR0_EL1.get();
    PhysAddr::from(val as usize)
}

/// Writes the register to update the current page table root for kernel space.
///
/// When the `arm-el2` feature is enabled, writes `TTBR0_EL2`; otherwise
/// writes `TTBR1_EL1`.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    #[cfg(not(feature = "arm-el2"))]
    {
        TTBR1_EL1.set(root_paddr.as_usize() as _);
    }

    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::TTBR0_EL2;
        TTBR0_EL2.set(root_paddr.as_usize() as _);
    }
}

/// Writes the register to update the current page table root for user space
/// (`TTBR0_EL1`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    TTBR0_EL1.set(root_paddr.as_usize() as _);
}
