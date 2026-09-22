// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local data structures.

use kcpu_slot::cpu_slot_cell;
pub use kplat::cpu::*;

cpu_slot_cell! {
    /// Owning raw pointer to this CPU's current task; `0` means no task.
    static CURRENT_TASK_PTR: usize = 0;
}

/// Gets the pointer to the current task with preemption-safety.
///
/// Preemption may be enabled when calling this function. This function will
/// guarantee the correctness even the current task is preempted.
///
/// The read uses the single-instruction current-CPU accessor (not a guard) so
/// it stays a short sequence on every architecture and can be called from
/// contexts that predate any guard — including the preempt-count callback.
#[inline]
pub fn current_task_ptr<T>() -> *const T {
    CURRENT_TASK_PTR.load_current() as *const T
}

/// Sets the pointer to the current task with preemption-safety.
///
/// Preemption may be enabled when calling this function. This function will
/// guarantee the correctness even the current task is preempted.
///
/// # Safety
///
/// The given `ptr` must be pointed to a valid task structure.
#[inline]
pub unsafe fn set_current_task_ptr<T>(ptr: *const T) {
    // SAFETY: the caller guarantees `ptr` targets a valid task structure, and
    // the single-instruction store stays on this CPU's slot.
    CURRENT_TASK_PTR.store_current(ptr as usize);
}
#[cfg(feature = "smp")]
pub use kplat::cpu::ap_cpu_init as init_secondary;
pub use kplat::cpu::{boot_cpu_init as init_primary, id as this_cpu_id};

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_percpu {
    use unittest::def_test;

    use super::{current_task_ptr, this_cpu_id};

    #[def_test]
    fn test_current_task_ptr_consistent() {
        let first = current_task_ptr::<u8>() as usize;
        let second = current_task_ptr::<u8>() as usize;
        assert_eq!(first, second);
    }

    #[def_test]
    fn test_this_cpu_id_consistent() {
        let id = this_cpu_id();
        assert_eq!(id, this_cpu_id());
    }
}
