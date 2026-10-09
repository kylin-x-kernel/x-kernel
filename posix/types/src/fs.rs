// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing filesystem structures.

use core::mem::{align_of, offset_of, size_of};

use linux_raw_sys::general::{__kernel_loff_t, __kernel_pid_t, flock64, stat, statfs, statx};

use crate::{UserRead, UserWrite};

/// A `struct flock64` carrier used at the syscall boundary.
///
/// `linux_raw_sys::general::flock64` has four bytes of implicit padding after
/// `l_whence` and four trailing bytes on 64-bit targets. Copying that type to
/// user memory would expose uninitialized kernel bytes, so this carrier models
/// both gaps as explicit fields. A value read from user memory keeps the
/// caller's bytes in them, as Linux does when it writes the lock back.
#[repr(C)]
#[derive(Clone, Copy, Debug, UserRead, UserWrite)]
pub struct k_flock64 {
    /// Lock type: `F_RDLCK`, `F_WRLCK`, or `F_UNLCK`.
    pub l_type: i16,
    /// How `l_start` is interpreted: `SEEK_SET`, `SEEK_CUR`, or `SEEK_END`.
    pub l_whence: i16,
    abi_pad: u32,
    /// Starting offset of the lock.
    pub l_start: __kernel_loff_t,
    /// Number of bytes to lock; zero means until end of file.
    pub l_len: __kernel_loff_t,
    /// Process holding the conflicting lock, for `F_GETLK`.
    pub l_pid: __kernel_pid_t,
    abi_pad_tail: u32,
}

const _: () = {
    assert!(size_of::<k_flock64>() == size_of::<flock64>());
    assert!(align_of::<k_flock64>() == align_of::<flock64>());
    assert!(offset_of!(k_flock64, l_whence) == offset_of!(flock64, l_whence));
    assert!(offset_of!(k_flock64, l_start) == offset_of!(flock64, l_start));
    assert!(offset_of!(k_flock64, l_len) == offset_of!(flock64, l_len));
    assert!(offset_of!(k_flock64, l_pid) == offset_of!(flock64, l_pid));
};

// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for stat {}
// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for statfs {}
// SAFETY: these Linux ABI structs are plain-old-data syscall carriers whose
// byte representations may be copied across the user/kernel boundary as declared.
unsafe impl UserWrite for statx {}
