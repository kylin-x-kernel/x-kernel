// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel current lives in `$tp`; user entry preserves it separately.

/// Observes the current task, or null before scheduler initialization.
///
/// Only kernel execution may use this accessor. Boot clears `$tp`, and user
/// exit restores the kernel value before calling Rust. This observation
/// neither masks IRQs nor acquires ownership. Do not cache it or pass it to
/// another CPU: continued use needs an independent task-lifetime guarantee.
#[inline]
pub fn current_task_ptr() -> *const () {
    let ptr: *const ();
    // SAFETY: Kernel entry establishes tp before Rust runs. Reading it does
    // not dereference the borrowed pointer or change CPU state.
    unsafe {
        core::arch::asm!("move {}, $tp", out(reg) ptr, options(nostack, preserves_flags));
    }
    ptr
}

/// Publishes current to local kernel observers without taking ownership.
///
/// # Safety
///
/// The caller must exclude scheduling and normal IRQs throughout replacement.
/// Retain the initialized new task while current and the old task until its
/// stack and local borrowed observers are no longer in use. Any interrupting
/// observer must finish before replacement resumes. This does not establish
/// a user-mode entry protocol for an NMI implementation.
#[inline]
pub unsafe fn set_current_task_ptr(ptr: *const ()) {
    // SAFETY: The caller retains both tasks and excludes migration. Kernel
    // code does not use TLS; the user thread pointer is restored separately.
    unsafe {
        core::arch::asm!("move $tp, {}", in(reg) ptr, options(nostack, preserves_flags));
    }
}
