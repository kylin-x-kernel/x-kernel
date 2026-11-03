// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing task and capability structure traits.

use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};

use crate::ptr::{UserRead, UserWrite};

/// The two `__user_cap_data_struct` words used by capability ABI versions 2/3.
///
/// Versions 2 and 3 address capabilities `0..63` with two consecutive data
/// words. Modeling both words as one `repr(C)` carrier lets `capget(2)` and
/// `capset(2)` copy the complete ABI object without a hand-written scatter
/// loop. Version 1 callers copy only the first `__user_cap_data_struct`;
/// `ksyscall` narrows that transfer rather than changing this carrier.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CapUserData {
    /// Effective, permitted and inheritable capabilities `0..31`.
    pub low: __user_cap_data_struct,
    /// Effective, permitted and inheritable capabilities `32..63`.
    pub high: __user_cap_data_struct,
}

// The ABI object is exactly two `__user_cap_data_struct` words (24 bytes);
// guard against accidental field or layout changes drifting from capget(2).
const _: () = assert!(
    core::mem::size_of::<CapUserData>() == 2 * core::mem::size_of::<__user_cap_data_struct>()
);
const _: () = assert!(core::mem::size_of::<CapUserData>() == 24);

// SAFETY: `CapUserData` is `repr(C)` over two `#[repr(C)]`
// `__user_cap_data_struct` values. Each field is three `u32`s, so the carrier
// has no implicit padding and every byte pattern is a valid value.
unsafe impl UserRead for CapUserData {}
// SAFETY: `CapUserData` has no implicit padding, and every field of both
// `__user_cap_data_struct` words is always initialized. Its byte representation
// is therefore fully initialized for copy-to-user.
unsafe impl UserWrite for CapUserData {}

// SAFETY: `__user_cap_header_struct` is a `#[repr(C)]` pair of `u32`/`i32`
// fields with no padding, so every byte pattern is a valid value.
unsafe impl UserRead for __user_cap_header_struct {}

// SAFETY: `__user_cap_data_struct` is a `#[repr(C)]` triple of `u32` fields
// with no padding; every byte pattern is a valid value.
unsafe impl UserRead for __user_cap_data_struct {}
// SAFETY: `__user_cap_data_struct` is a `#[repr(C)]` triple of `u32` fields
// with no padding, so its bytes are fully initialized.
unsafe impl UserWrite for __user_cap_data_struct {}
// SAFETY: `__user_cap_header_struct` is a `#[repr(C)]` pair of `u32`/`i32`
// fields with no padding, so its bytes are fully initialized.
unsafe impl UserWrite for __user_cap_header_struct {}
