// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX signal ABI types.

use core::{ffi::c_ulong, mem};

use linux_raw_sys::general::{__kernel_sighandler_t, __sigrestore_t, kernel_sigset_t, siginfo_t};

use crate::{UserRead, UserWrite};

/// A raw `sigset_t` carrier used at the syscall boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
#[allow(non_camel_case_types)]
pub struct k_sigset(pub u64);

impl From<k_sigset> for kernel_sigset_t {
    fn from(value: k_sigset) -> Self {
        // SAFETY: `kernel_sigset_t` has the same layout as `[c_ulong; 1]`.
        unsafe { mem::transmute::<u64, kernel_sigset_t>(value.0) }
    }
}

impl From<kernel_sigset_t> for k_sigset {
    fn from(value: kernel_sigset_t) -> Self {
        // SAFETY: `kernel_sigset_t` has the same layout as `[c_ulong; 1]`.
        Self(unsafe { mem::transmute::<kernel_sigset_t, u64>(value) })
    }
}

unsafe impl UserRead for k_sigset {}
unsafe impl UserWrite for k_sigset {}

/// A raw `sigaction` carrier used at the syscall boundary.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct k_sigaction {
    pub handler: __kernel_sighandler_t,
    pub flags: c_ulong,
    pub restorer: __sigrestore_t,
    pub mask: k_sigset,
}

unsafe impl UserRead for k_sigaction {}
unsafe impl UserWrite for k_sigaction {}

/// A raw `siginfo_t` carrier used at the syscall boundary.
#[derive(Clone)]
#[repr(transparent)]
#[allow(non_camel_case_types)]
pub struct k_siginfo(pub siginfo_t);

unsafe impl UserRead for k_siginfo {}
unsafe impl UserWrite for k_siginfo {}

/// A raw `sigaltstack` carrier used at the syscall boundary.
#[derive(Clone, Copy)]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct k_sigaltstack {
    pub sp: usize,
    pub flags: u32,
    pub abi_pad: u32,
    pub size: usize,
}

unsafe impl UserRead for k_sigaltstack {}
unsafe impl UserWrite for k_sigaltstack {}
