// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::default::Default;

/// user ta context
/// NOTE: NEVER USE THIS STRUCT IN YOUR CODE
#[repr(C)]
pub struct user_ta_ctx {}

impl Default for user_ta_ctx {
    fn default() -> Self {
        Self {}
    }
}
