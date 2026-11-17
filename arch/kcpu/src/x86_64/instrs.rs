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
    #[link_name = "user_copy"]
    fn user_copy_asm(dst: *mut u8, src: *const u8, size: usize) -> usize;
    #[link_name = "user_atomic_load_u32"]
    fn user_atomic_load_u32_asm(addr: *const u32, value_out: *mut u32) -> usize;
    #[link_name = "user_atomic_cmpxchg_u32"]
    fn user_atomic_cmpxchg_u32_asm(addr: *mut u32, old: u32, new: u32, old_out: *mut u32) -> usize;
}

/// Copies bytes between kernel and user memory in either direction.
///
/// The source range must be readable and the destination range writable for
/// `size` bytes. Whichever range lies in user space must be valid in the active
/// address space. The caller selects the direction by which pointer is in
/// user space.
///
/// Returns the number of bytes not copied: zero means the whole range was
/// copied; a nonzero value reports the uncopied remainder after a fault.
///
/// # Safety
/// `dst` must be writable and `src` readable for `size` bytes. Any user-space
/// range must be valid in the active address space.
#[inline]
pub unsafe fn user_copy(dst: *mut u8, src: *const u8, size: usize) -> usize {
    // SAFETY: The caller upholds the raw copy's pointer and address-space contract.
    unsafe { user_copy_asm(dst, src, size) }
}

/// Atomically loads a naturally aligned 32-bit word from user memory.
///
/// Returns zero when no fault occurred and nonzero on fault. `value_out` is
/// written with the loaded value on success.
///
/// # Safety
/// `addr` must be 4-byte aligned and readable in the active address space;
/// `value_out` must point to writable kernel memory.
#[inline]
pub unsafe fn user_atomic_load_u32(addr: *const u32, value_out: *mut u32) -> usize {
    // SAFETY: The caller upholds the user address and output pointer contract.
    unsafe { user_atomic_load_u32_asm(addr, value_out) }
}

/// Atomically compare-exchanges a naturally aligned 32-bit user word.
///
/// On success (`0`), writes the previous value to `old_out`, and stores `new`
/// if that value equals `old`. A nonzero return indicates a fault; the output
/// value is valid only on success.
///
/// # Safety
/// `addr` must be 4-byte aligned and accessible in the active address space
/// under the user-access window; `old_out` must point to writable kernel memory.
#[inline]
pub unsafe fn user_atomic_cmpxchg_u32(
    addr: *mut u32,
    old: u32,
    new: u32,
    old_out: *mut u32,
) -> usize {
    // SAFETY: The caller upholds the user address and output pointer contract.
    unsafe { user_atomic_cmpxchg_u32_asm(addr, old, new, old_out) }
}
