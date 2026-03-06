// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMU/page table operations for LoongArch64.

use core::arch::asm;

use loongArch64::register::{pgdh, pgdl};
use memaddr::PhysAddr;

/// Reads the current page table root register for user space (`PGDL`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    PhysAddr::from(pgdl::read().base())
}

/// Reads the current page table root register for kernel space (`PGDH`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    PhysAddr::from(pgdh::read().base())
}

/// Writes the register to update the current page table root for user space
/// (`PGDL`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    pgdl::set_base(root_paddr.as_usize() as _);
}

/// Writes the register to update the current page table root for kernel space
/// (`PGDH`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    pgdh::set_base(root_paddr.as_usize());
}

/// Writes the Page Walk Controller registers (`PWCL` and `PWCH`).
///
/// The CSR numbers are inlined as numeric constants:
/// - `PWCL` = CSR 0x1c (lower-half page walk controller)
/// - `PWCH` = CSR 0x1d (higher-half page walk controller)
///
/// # Safety
///
/// This function is unsafe as it changes the page walk configuration such as
/// levels and starting bits.
///
/// - `PWCL` (CSR 0x1c): <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-lower-half-address-space>
/// - `PWCH` (CSR 0x1d): <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-higher-half-address-space>
#[inline]
pub unsafe fn write_pwc(pwcl: u32, pwch: u32) {
    unsafe {
        asm!(
            "csrwr {}, 0x1c",
            "csrwr {}, 0x1d",
            in(reg) pwcl,
            in(reg) pwch
        )
    }
}
