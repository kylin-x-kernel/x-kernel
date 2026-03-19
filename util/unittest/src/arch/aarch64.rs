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
