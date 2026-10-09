// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX signal ABI types.

use core::ffi::c_ulong;

use kerrno::{KError, KResult};
use linux_raw_sys::general::{kernel_sigaction, kernel_sigset_t, sigevent, siginfo_t, sigval_t};

use crate::{UserRead, UserWrite};

/// A raw `sigset_t` carrier used at the syscall boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, UserRead, UserWrite)]
#[repr(transparent)]
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub struct k_sigset(
    /// Raw signal bits; bit zero represents signal 1.
    pub u64,
);

/// Accepts zero or the eight-byte kernel `sigset_t` size.
///
/// # Errors
///
/// Returns `InvalidInput` for any other size. A zero size is intentionally
/// accepted here; syscall-specific requirements belong to the adapter.
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
///
/// Reuses the target Linux definition, including its optional restorer slot,
/// so syscall copies cannot diverge from the architecture's field layout.
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub type k_sigaction = kernel_sigaction;

// SAFETY: the target ABI record contains only nullable function addresses and
// integer flags/mask words. This crate copies, but never invokes, the addresses.
unsafe impl UserRead for k_sigaction {}
// SAFETY: on supported 64-bit targets every field is a fully initialized
// pointer-sized value or mask word; the ABI record has no implicit padding.
unsafe impl UserWrite for k_sigaction {}

/// A raw `siginfo_t` carrier used at the syscall boundary.
#[derive(Clone, UserRead, UserWrite)]
#[repr(transparent)]
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub struct k_siginfo(
    /// Raw Linux signal-information record, including its tagged union payload.
    pub siginfo_t,
);

/// A raw `sigval_t` carrier used at the syscall boundary.
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub type k_sigval = sigval_t;

// SAFETY: `sigval_t` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigval {}
// SAFETY: `sigval_t` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserWrite for k_sigval {}

/// A raw `sigevent` carrier used at the syscall boundary.
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub type k_sigevent = sigevent;

// SAFETY: `sigevent` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigevent {}
// SAFETY: `sigevent` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserWrite for k_sigevent {}

/// A raw `sigaltstack` carrier used at the syscall boundary.
#[derive(Clone, Copy, UserRead, UserWrite)]
#[repr(C)]
pub struct k_sigaltstack {
    /// User virtual address of the alternate stack base.
    pub sp: usize,
    /// Raw ABI flags, validated by the consuming signal adapter.
    pub flags: u32,
    /// Explicit padding; initialize before copying to user space.
    pub abi_pad: u32,
    /// Alternate stack size in bytes.
    pub size: usize,
}
