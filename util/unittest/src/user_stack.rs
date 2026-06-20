// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{TestDescriptor, TestResult, arch};

#[inline]
fn encode_test_result(result: TestResult) -> u8 {
    match result {
        TestResult::Ok => 0,
        TestResult::Failed => 1,
        TestResult::Ignored => 2,
    }
}

#[inline]
fn decode_test_result(code: u8) -> TestResult {
    match code {
        0 => TestResult::Ok,
        1 => TestResult::Failed,
        2 => TestResult::Ignored,
        _ => TestResult::Failed,
    }
}

extern "C" fn test_entry(test: *const TestDescriptor) -> u8 {
    // SAFETY: `test_entry` is only called from `call_on_stack`, which passes a
    // valid pointer to a live `TestDescriptor`.
    let result = unsafe { ((*test).test_fn)() };
    encode_test_result(result)
}

/// Run one test function on the provided temporary stack.
///
/// # Safety
///
/// - `stack_top` must point to a valid writable stack region dedicated to this call.
/// - The stack region below `stack_top` must have enough space for the target architecture's
///   call frame and temporary data used by `arch::call_on_stack`.
/// - The caller must ensure no concurrent use of the same stack memory.
/// - `test` must remain valid for the whole duration of this call.
pub unsafe fn run_test_on_user_stack(test: &TestDescriptor, stack_top: usize) -> TestResult {
    // SAFETY: The caller upholds the temporary-stack contract, and `test`
    // remains live for the duration of the stack-switched call.
    let code = unsafe { arch::call_on_stack(stack_top, test_entry, test as *const TestDescriptor) };
    decode_test_result(code)
}
