// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::StackTestEntry;
use crate::TestDescriptor;

/// Fallback implementation that invokes `entry` without switching stacks.
///
/// # Safety
///
/// `entry` must tolerate running on the current stack, and `test` must remain
/// valid for the duration of the call.
pub unsafe fn call_on_stack(
    _stack_top: usize,
    entry: StackTestEntry,
    test: *const TestDescriptor,
) -> u8 {
    warn!(
        "def_test(user) fallback to direct execution on unsupported arch for {}:{}",
        // SAFETY: the caller guarantees `test` points to a live test
        // descriptor for the duration of this fallback invocation.
        unsafe { (*test).module },
        // SAFETY: same as above; `test` remains valid while formatting the warning.
        unsafe { (*test).name }
    );
    entry(test)
}
