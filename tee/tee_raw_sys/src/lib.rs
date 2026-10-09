// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![expect(
    non_camel_case_types,
    non_snake_case,
    reason = "CG-024-lint-exceptions: non_camel_case_types, non_snake_case: Expose GlobalPlatform \
              TEE C ABI type and field names verbatim"
)]

pub use tee_api_defines::*;
pub use tee_api_types::*;
pub use utee_types::*;

mod tee_api_defines;
#[expect(
    clippy::upper_case_acronyms,
    reason = "CG-032-camel-case-types: raw GlobalPlatform TEE ABI bindings retain the external C \
              type and enum names"
)]
mod tee_api_types;
#[expect(
    clippy::upper_case_acronyms,
    reason = "CG-032-camel-case-types: raw OP-TEE syscall ABI bindings retain the external \
              operation and time-category names"
)]
mod utee_types;

/// Libc compatibility types
pub mod libc_compat {
    /// C size type
    pub type size_t = usize;
    /// Maximum-width integer type
    pub type intmax_t = i64;
}
