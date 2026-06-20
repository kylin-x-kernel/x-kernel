// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMU/page table operations for x86_64.

use memaddr::{MemoryAddr, PhysAddr};
use x86::controlregs;

const SEV_CBIT_MASK: usize = if kbuild_config::SEV_CBIT_POS == 0 {
    0
} else {
    1usize << kbuild_config::SEV_CBIT_POS
};

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
        HwPageTableRoot::new(root_paddr.as_usize() | SEV_CBIT_MASK)
    }
}

/// Reads the current page table root register for user space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_kernel_page_table`].
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_user_page_table() -> HwPageTableRoot {
    // SAFETY: reading `cr3` is side-effect free and returns the current page-table root.
    HwPageTableRoot::new((unsafe { controlregs::cr3() } as usize).align_down_4k())
}

/// Reads the current page table root register for kernel space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_user_page_table`].
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_kernel_page_table() -> HwPageTableRoot {
    read_user_page_table()
}

/// Writes the register to update the current page table root for user space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_kernel_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root: HwPageTableRoot) {
    // SAFETY: the caller guarantees `root` names a valid hardware page-table root
    // for the current address-space switch.
    unsafe { controlregs::cr3_write(root.as_usize() as _) }
}

/// Writes the register to update the current page table root for kernel space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_user_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root: HwPageTableRoot) {
    // SAFETY: x86_64 uses the same `cr3` root for kernel and user mappings, so
    // this forwards the caller's validated root unchanged.
    unsafe { write_user_page_table(root) }
}
