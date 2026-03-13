// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;
use core::ffi::c_char;

use kerrno::{KError, KResult};
use osvm::{load_vec, load_vec_until_null};

pub fn vm_load_string(ptr: *const c_char) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = load_vec_until_null(ptr as *const u8)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

pub fn vm_load_string_with_len(ptr: *const c_char, len: usize) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = load_vec(ptr as *const u8, len)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}
