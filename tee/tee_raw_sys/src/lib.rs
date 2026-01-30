// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![allow(non_camel_case_types, non_snake_case)]

pub use tee_api_defines::*;
pub use tee_api_types::*;
pub use utee_types::*;

mod tee_api_defines;
mod tee_api_types;
mod utee_types;

/// Libc compatibility types
pub mod libc_compat {
    /// C size type
    pub type size_t = usize;
    /// Maximum-width integer type
    pub type intmax_t = i64;
}
