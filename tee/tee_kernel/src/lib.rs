// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: case (b): 21 legacy outer allowances need individual \
              contract review; new code must prefer narrow expect attributes and justify \
              cfg-dependent allowances"
)]
#![allow(
    clippy::allow_attributes_without_reason,
    reason = "CG-024-lint-exceptions: clippy::allow_attributes_without_: case (b): 22 legacy \
              sites across TEE crypto, storage and protocol code need individual review; new code \
              must use narrow lint exceptions with substantive reasons"
)]
#![no_std]
#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#![allow(missing_docs)]

#[macro_use]
extern crate klogger;

extern crate alloc;

pub mod file;
pub mod mm;
pub mod tee;
pub use tee_raw_sys::TEE_SUCCESS;
#[cfg(unittest)]
pub use unittest_support::{TestUserArray, TestUserBuffer, TestUserValue, user_vec};
