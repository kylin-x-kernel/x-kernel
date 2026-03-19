// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::StackTestEntry;
use crate::TestDescriptor;

pub unsafe fn call_on_stack(
    stack_top: usize,
    entry: StackTestEntry,
    test: *const TestDescriptor,
) -> u8 {
    let ret: usize;
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
