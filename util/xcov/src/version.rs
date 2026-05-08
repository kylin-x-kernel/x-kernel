// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profile version variable.

use crate::types::INSTR_PROF_RAW_VERSION;

/// Raw profile version (base only, no variant masks).
#[used]
static LLVM_PROFILE_RAW_VERSION: u64 = INSTR_PROF_RAW_VERSION;

/// Returns the profile version value.
pub fn get_raw_version() -> u64 {
    // SAFETY: Static variable, always valid to read.
    unsafe { core::ptr::read_volatile(&LLVM_PROFILE_RAW_VERSION) }
}
