// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::StackTestEntry;
use crate::TestDescriptor;

/// Invoke `entry` on an alternate stack and pass it `test`.
///
/// # Safety
///
/// `stack_top` must point to the top of a valid writable temporary stack
/// region reserved for this call, `entry` must follow the expected ABI, and
/// `test` must remain valid for the duration of the stack-switched call.
pub unsafe fn call_on_stack(
    stack_top: usize,
    entry: StackTestEntry,
    test: *const TestDescriptor,
) -> u8 {
    let ret: usize;
    // SAFETY: The caller guarantees `stack_top` points to writable stack
    // memory dedicated to this call, and `entry` preserves the saved stack
    // pointer before returning.
    unsafe {
        core::arch::asm!(
            "mov x9, {stack_top}",
            "and x9, x9, #-16",
            "sub x9, x9, #16",
            "mov x10, sp",
            "str x10, [x9]",
            "mov sp, x9",
            "blr {entry}",
            "ldr x10, [sp]",
            "mov sp, x10",
            stack_top = in(reg) stack_top,
            entry = in(reg) entry,
            in("x0") test,
            lateout("x0") ret,
            out("x9") _,
            out("x10") _,
            clobber_abi("C"),
        );
    }
    ret as u8
}
