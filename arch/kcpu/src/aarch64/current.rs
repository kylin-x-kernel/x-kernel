// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel current lives in SP_EL0; EL0 entry recovers it from ENTRY_TASK.

use core::sync::atomic::Ordering;

kcpu_slot::cpu_slot! {
    static ENTRY_TASK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
}

pub(super) use ENTRY_TASK::TEMPLATE as ENTRY_TASK_TEMPLATE;

/// Observes the current task, or null before scheduler initialization.
///
/// Only kernel execution may use this accessor. Boot clears SP_EL0, and EL0
/// entry replaces the user stack pointer with current before calling Rust.
/// This single-register observation neither masks IRQs nor acquires ownership.
/// Do not cache it or pass it to another CPU: continued use needs an independent
/// lifetime guarantee. An NMI borrow must end before interrupted code resumes.
#[inline]
pub fn current_task_ptr() -> *const () {
    let ptr: *const ();
    // SAFETY: Kernel entry establishes SP_EL0 before Rust runs. Reading the
    // register does not dereference the borrowed pointer or change CPU state.
    unsafe {
        core::arch::asm!("mrs {}, sp_el0", out(reg) ptr, options(nostack, preserves_flags));
    }
    ptr
}

/// Publishes current and the pointer used to recover it on EL0 entry.
///
/// # Safety
///
/// The caller must exclude scheduling and normal IRQs throughout replacement,
/// on a CPU with initialized per-CPU storage. Retain the initialized new task
/// while current and the old task until its stack and local borrowed observers
/// are no longer in use. NMI may observe either task during publication and
/// must finish before replacement resumes. Neither pointer owns a reference.
#[inline]
pub unsafe fn set_current_task_ptr(ptr: *const ()) {
    // EL0 cannot run between these stores; a local NMI reads only SP_EL0.
    ENTRY_TASK
        .current_shared()
        .store(ptr as usize, Ordering::Release);
    // SAFETY: The caller retains both tasks and excludes migration. This is
    // kernel execution, so SP_EL0 is not currently the user stack pointer.
    unsafe {
        core::arch::asm!("msr sp_el0, {}", in(reg) ptr, options(nostack, preserves_flags));
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(
    current_unittest,
    "../../ut-tests/aarch64_current_unittest.rs"
);
