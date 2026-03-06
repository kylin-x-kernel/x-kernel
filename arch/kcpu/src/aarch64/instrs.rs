// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrapper functions for assembly instructions.

use memaddr::PhysAddr;

/// Deprecated: use [`karch::read_kernel_page_table`] instead.
#[deprecated(note = "Use `karch::read_kernel_page_table` instead")]
#[inline]
pub fn kernel_pt_root() -> PhysAddr {
    karch::read_kernel_page_table()
}

/// Deprecated: use [`karch::read_user_page_table`] instead.
#[deprecated(note = "Use `karch::read_user_page_table` instead")]
#[inline]
pub fn user_pt_root() -> PhysAddr {
    karch::read_user_page_table()
}

/// Deprecated: use [`karch::write_trap_vector_base`] instead.
#[deprecated(note = "Use `karch::write_trap_vector_base` instead")]
#[inline]
pub unsafe fn write_exception_vector_base(vbar: usize) {
    unsafe { karch::write_trap_vector_base(vbar) }
}

#[cfg(feature = "uspace")]
core::arch::global_asm!(include_str!("copy_user.S"));

#[cfg(feature = "uspace")]
unsafe extern "C" {
    /// Copies data from source to destination, where addresses may be in user
    /// space. Equivalent to memcpy.
    ///
    /// # Safety
    /// This function is unsafe because it performs raw memory operations.
    ///
    /// # Returns
    /// Returns the number of bytes not copied. This means 0 indicates success,
    /// while a value > 0 indicates failure.
    pub fn raw_copy_from_user(dst: *mut u8, src: *const u8, size: usize) -> usize;
}

/// Alias for compatibility with other architectures
#[cfg(feature = "uspace")]
pub use raw_copy_from_user as user_copy;

