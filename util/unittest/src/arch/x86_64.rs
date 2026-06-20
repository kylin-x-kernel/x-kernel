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
    // SAFETY: the caller provides a valid alternate stack top and test entry;
    // the assembly switches to that stack, preserves the original `rsp`, and
    // returns control using the normal x86_64 C ABI.
    unsafe {
        core::arch::asm!(
            "mov r10, {stack_top}",
            "and r10, -16",
            // Keep 128-byte red-zone below rsp available for compiler-generated accesses.
            "sub r10, 128",
            "mov [r10], rsp",
            "mov rsp, r10",
            "call {entry}",
            "mov rsp, [rsp]",
            stack_top = in(reg) stack_top,
            entry = in(reg) entry,
            in("rdi") test,
            lateout("rax") ret,
            out("r10") _,
            clobber_abi("C"),
        );
    }
    ret as u8
}
