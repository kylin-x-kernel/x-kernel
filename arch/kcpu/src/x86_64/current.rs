// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Current-task identity in a GS-relative slot. Only the scheduler owns tasks;
//! this module stores and observes an opaque pointer without acquiring a ref.

kcpu_slot::cpu_slot! {
    static CURRENT_TASK_PTR: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

/// Observes the current task, or null before scheduler initialization.
///
/// One GS-relative instruction selects the CPU and reads the aligned pointer
/// without changing IRQ or preemption state. Kernel GS must already select
/// this CPU's initialized area. The returned pointer has no acquired ownership
/// or continuing CPU pin: dereferencing, caching or transferring it requires an
/// independent task-lifetime guarantee. An NMI borrow must end before returning
/// to interrupted execution.
#[inline]
pub fn current_task_ptr() -> *const () {
    let ptr: usize;
    // SAFETY: kernel GS selects initialized storage. The offset names an
    // aligned pointer-sized slot, accessed only by these GS loads/stores.
    // One instruction cannot straddle migration or local NMI; x86 load
    // ordering and the compiler memory clobber preserve observation ordering.
    unsafe {
        core::arch::asm!(
            "mov {}, qword ptr gs:[offset {slot}]",
            out(reg) ptr,
            slot = sym CURRENT_TASK_PTR::TEMPLATE,
            options(nostack, preserves_flags),
        );
    }
    ptr as *const ()
}

/// Publishes an initialized current-task pointer to local observers.
///
/// # Safety
///
/// The scheduler must exclude scheduling and normal IRQs across the entire
/// replacement transaction, with kernel GS selecting this CPU's initialized
/// area. Retain the new task while it is current, the old task through
/// publication, and either task until its stack and borrowed observations are
/// no longer in use. Local NMI may borrow either task but must finish before
/// the interrupted replacement resumes. No reference is acquired or released
/// by this function.
#[inline]
pub unsafe fn set_current_task_ptr(ptr: *const ()) {
    // SAFETY: the caller keeps kernel GS stable and both tasks live through
    // publication. The aligned GS store publishes one complete pointer.
    // x86 store ordering and the compiler memory clobber keep initialization
    // before publication and old-owner release after it.
    unsafe {
        core::arch::asm!(
            "mov qword ptr gs:[offset {slot}], {}",
            in(reg) ptr,
            slot = sym CURRENT_TASK_PTR::TEMPLATE,
            options(nostack, preserves_flags),
        );
    }
}
