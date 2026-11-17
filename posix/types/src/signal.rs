// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX signal ABI types.

use core::ffi::c_ulong;

use kerrno::{KError, KResult};
use linux_raw_sys::general::{kernel_sigaction, kernel_sigset_t, sigevent, siginfo_t, sigval_t};

use crate::{UserRead, UserWrite};

/// A raw `sigset_t` carrier used at the syscall boundary.
#[repr(transparent)]
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, UserRead, UserWrite)]
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
///
/// The union payload may have members smaller than the whole record. Kernel
/// output must originate from `ksignal::SignalInfo`, whose constructor zeroes
/// the complete `siginfo_t` before setting individual members.
#[repr(transparent)]
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
#[derive(Clone, UserRead)]
pub struct k_siginfo(
    /// Raw Linux signal-information record, including its tagged union payload.
    siginfo_t,
);

impl k_siginfo {
    /// Returns the fully initialized Linux signal-information record.
    pub fn into_raw(self) -> siginfo_t {
        self.0
    }

    /// Returns the complete initialized 128-byte Linux `siginfo_t` record.
    pub fn into_bytes(self) -> [u8; 128] {
        // SAFETY: k_siginfo's UserWrite contract requires every byte to be
        // initialized, including inactive union arms; both types are 128 bytes.
        unsafe { core::mem::transmute(self.0) }
    }

    /// Wraps a raw record whose complete byte representation is initialized.
    ///
    /// # Safety
    ///
    /// `raw` must have every byte initialized, including unused union arms.
    /// Construct it from an all-zero record before setting union members, or
    /// obtain it from a complete user-memory copy.
    pub unsafe fn from_initialized_raw(raw: siginfo_t) -> Self {
        Self(raw)
    }
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    unsound_user_abi_impl,
    reason = "CG-005-user-abi-layout: SignalInfo::empty zeroes the full union record"
)]
// SAFETY: the raw field is private. Safe construction is through UserRead,
// which copies every byte. The unsafe constructor requires an all-initialized
// record; ksignal::SignalInfo starts from a zeroed record before member writes.
unsafe impl UserWrite for k_siginfo {}

/// A raw `sigval_t` carrier read at the syscall boundary.
///
/// A value initialized through `sival_int` may leave the upper union bytes
/// uninitialized, so this type does not implement `UserWrite`.
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub type k_sigval = sigval_t;

// SAFETY: `sigval_t` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigval {}

/// A raw `sigevent` carrier read at the syscall boundary.
///
/// Its nested `sigval_t` has the same partial-initialization risk.
#[expect(
    non_camel_case_types,
    reason = "CG-032-camel-case-types: Preserve the Linux ABI type name used by syscall records"
)]
pub type k_sigevent = sigevent;

// SAFETY: `sigevent` is an ABI POD type copied verbatim at the syscall boundary.
unsafe impl UserRead for k_sigevent {}

/// A raw `sigaltstack` carrier used at the syscall boundary.
#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
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

const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    use linux_raw_sys::general::stack_t;

    assert!(size_of::<k_sigaltstack>() == size_of::<stack_t>());
    assert!(align_of::<k_sigaltstack>() == align_of::<stack_t>());
    assert!(offset_of!(k_sigaltstack, sp) == offset_of!(stack_t, ss_sp));
    assert!(offset_of!(k_sigaltstack, flags) == offset_of!(stack_t, ss_flags));
    assert!(offset_of!(k_sigaltstack, size) == offset_of!(stack_t, ss_size));
};
