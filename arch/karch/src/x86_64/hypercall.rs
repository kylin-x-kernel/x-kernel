// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hypervisor call operations for x86_64.

use core::arch::asm;

/// Performs a hypercall to the hypervisor using the `vmmcall` instruction.
///
/// This is used on AMD/Hygon platforms for KVM hypercalls.
/// For Intel platforms, `vmcall` would be used instead.
///
/// # Arguments
/// * `nr` - Hypercall number (passed in RAX)
/// * `a0` - First argument (passed in RBX)
/// * `a1` - Second argument (passed in RCX)
///
/// # Returns
/// The return value from the hypervisor (from RAX).
#[inline]
pub fn hypercall(nr: u64, a0: u64, a1: u64) -> i64 {
    let ret: i64;
    // SAFETY: this follows the hypervisor ABI for `vmmcall`, preserves `rbx`
    // across the inline assembly block, and only clobbers the documented registers.
    unsafe {
        // Note: rbx is reserved by LLVM, so we need to save/restore it manually
        asm!(
            "push rbx",
            "mov rbx, {a0}",
            "vmmcall",
            "pop rbx",
            a0 = in(reg) a0,
            inout("rax") nr => ret,
            in("rcx") a1,
            options()
        );
    }
    ret
}
