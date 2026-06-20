// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX signal ABI types.

use core::ffi::c_ulong;

use kerrno::{KError, KResult};
use linux_raw_sys::general::{
    __kernel_sighandler_t, __sigrestore_t, kernel_sigset_t, sigevent, siginfo_t, sigval_t,
};

use crate::{UserRead, UserWrite};

/// A raw `sigset_t` carrier used at the syscall boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, UserRead, UserWrite)]
#[repr(transparent)]
#[allow(non_camel_case_types)]
pub struct k_sigset(pub u64);

/// Validates that an ABI `sigset_t` size matches the kernel expectation.
pub fn check_sigset_size(size: usize) -> KResult<()> {
    if size != size_of::<k_sigset>() && size != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

impl From<k_sigset> for kernel_sigset_t {
    fn from(value: k_sigset) -> Self {
        Self {
            sig: [value.0 as c_ulong],
        }
    }
}

impl From<kernel_sigset_t> for k_sigset {
    fn from(value: kernel_sigset_t) -> Self {
        Self(value.sig[0])
    }
}

/// A raw `sigaction` carrier used at the syscall boundary.
#[derive(Debug, Clone, Copy, UserRead, UserWrite)]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct k_sigaction {
    pub handler: __kernel_sighandler_t,
    pub flags: c_ulong,
    pub restorer: __sigrestore_t,
    pub mask: k_sigset,
}

/// A raw `siginfo_t` carrier used at the syscall boundary.
#[derive(Clone, UserRead, UserWrite)]
#[repr(transparent)]
#[allow(non_camel_case_types)]
pub struct k_siginfo(pub siginfo_t);

/// A raw `sigval_t` carrier used at the syscall boundary.
#[allow(non_camel_case_types)]
pub type k_sigval = sigval_t;

// SAFETY: `sigval_t` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigval {}
// SAFETY: `sigval_t` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserWrite for k_sigval {}

/// A raw `sigevent` carrier used at the syscall boundary.
#[allow(non_camel_case_types)]
pub type k_sigevent = sigevent;

// SAFETY: `sigevent` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigevent {}
// SAFETY: `sigevent` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserWrite for k_sigevent {}

/// A raw `sigaltstack` carrier used at the syscall boundary.
#[derive(Clone, Copy, UserRead, UserWrite)]
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct k_sigaltstack {
    pub sp: usize,
    pub flags: u32,
    pub abi_pad: u32,
    pub size: usize,
}
