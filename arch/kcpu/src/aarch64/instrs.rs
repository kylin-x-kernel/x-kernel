// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrapper functions for assembly instructions.

core::arch::global_asm!(
    include_str!("../asm/extable.inc"),
    include_str!("copy_user.S"),
    include_str!("atomic_user.S"),
);

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

    /// Atomically loads a naturally aligned 32-bit word from `addr`.
    ///
    /// On success (return `0`), writes the loaded value into `value_out`.
    ///
    /// # Safety
    ///
    /// `addr` must be a 4-byte-aligned address that is currently readable in
    /// the active address space. `value_out` must point to writable kernel
    /// memory.
    pub fn user_atomic_load_u32(addr: *const u32, value_out: *mut u32) -> usize;

    /// Atomically compare-exchanges a 32-bit word at `addr`.
    ///
    /// On success (return `0`), writes the previous `*addr` value into
    /// `old_out`. If that previous value equals `old`, stores `new`.
    ///
    /// # Safety
    ///
    /// `addr` must be a 4-byte-aligned address that is currently accessible in
    /// the active address space under the user-access window. `old_out` must
    /// point to writable kernel memory.
    ///
    /// # Returns
    ///
    /// `0` if no fault occurred; non-zero if a data abort occurred.
    pub fn user_atomic_cmpxchg_u32(addr: *mut u32, old: u32, new: u32, old_out: *mut u32) -> usize;
}

/// Alias for compatibility with other architectures
pub use raw_copy_from_user as user_copy;
