// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::StackTestEntry;
use crate::TestDescriptor;

pub unsafe fn call_on_stack(
    _stack_top: usize,
    entry: StackTestEntry,
    test: *const TestDescriptor,
) -> u8 {
    warn!(
        "def_test(user) fallback to direct execution on unsupported arch for {}:{}",
        unsafe { (*test).module },
        unsafe { (*test).name }
    );
    entry(test)
}
