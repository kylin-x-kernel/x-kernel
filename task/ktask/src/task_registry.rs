// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lock-free per-CPU task registry shared by snapshot and watchdog diagnostics.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use khal::percpu::this_cpu_id;

use crate::WeakKtaskRef;

/// Max number of task weak refs tracked per CPU for diagnostic dumps.
///
/// This is a best-effort debug facility: if it fills up, we will simply drop
/// new records and diagnostics may miss some tasks.
const TASK_REGISTRY_SLOTS: usize = 4096;

/// Lock-free per-CPU task registry shared by snapshot and watchdog diagnostics.
///
/// Safety / design notes:
/// - Writers (task creation + GC) run on the owning CPU, but NMI may read any CPU.
/// - Each slot stores a raw pointer to a heap-allocated `WeakKtaskRef` (usize).
/// - Readers snapshot the pointers with `Acquire` loads, dereference, and call
///   `upgrade()` (which is internally atomic).
/// - GC sweeps invalid weak refs and frees their boxes.
struct TaskRegistry {
    slots: [[AtomicUsize; TASK_REGISTRY_SLOTS]; kbuild_config::NR_CPUS],
}

impl TaskRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { [const { AtomicUsize::new(0) }; TASK_REGISTRY_SLOTS] };
                kbuild_config::NR_CPUS],
        }
    }

    #[inline]
    fn try_insert(&self, cpu_id: LogicalCpuId, weak: WeakKtaskRef) {
        let boxed = Box::new(weak);
        let ptr = Box::into_raw(boxed) as usize;

        for slot in &self.slots[cpu_id.as_usize()] {
            if slot
                .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }

        warn!("task registry on cpu {} is full!", cpu_id.as_usize());

        // SAFETY: `ptr` was produced by `Box::into_raw` just above and was not
        // published into any slot, so reconstructing and dropping the box is valid.
        unsafe { drop(Box::from_raw(ptr as *mut WeakKtaskRef)) };
    }

    #[inline]
    fn sweep_invalid(&self, cpu_id: LogicalCpuId) {
        for slot in &self.slots[cpu_id.as_usize()] {
            let ptr = slot.load(Ordering::Acquire);
            if ptr == 0 {
                continue;
            }
            // SAFETY: `ptr` is either 0 or a valid `Box<WeakKtaskRef>` raw
            // pointer installed by `try_insert` and not yet reclaimed.
            let weak = unsafe { &*(ptr as *const WeakKtaskRef) };
            if weak.upgrade().is_none()
                && slot
                    .compare_exchange(ptr, 0, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                // SAFETY: the successful CAS transfers sole ownership of
                // the boxed weak ref back to this CPU for reclamation.
                unsafe { drop(Box::from_raw(ptr as *mut WeakKtaskRef)) };
            }
        }
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64", feature = "watchdog"))]
    #[inline]
    fn for_each(&self, cpu_id: LogicalCpuId, mut f: impl FnMut(&WeakKtaskRef)) {
        for slot in &self.slots[cpu_id.as_usize()] {
            let ptr = slot.load(Ordering::Acquire);
            if ptr == 0 {
                continue;
            }
            // SAFETY: `ptr` is either 0 or a valid `Box<WeakKtaskRef>` raw
            // pointer published by `try_insert` and not reclaimed yet.
            let weak = unsafe { &*(ptr as *const WeakKtaskRef) };
            f(weak);
        }
    }
}

static TASK_REGISTRY: TaskRegistry = TaskRegistry::new();

/// Record a task into the current CPU's diagnostic task registry.
#[inline]
pub(crate) fn record_tracked_task(task: &crate::KtaskRef) {
    TASK_REGISTRY.try_insert(this_cpu_id(), Arc::downgrade(task));
}

/// Sweep invalid weak refs from the given CPU's diagnostic task registry.
#[inline]
pub(crate) fn sweep_tracked_tasks(cpu_id: LogicalCpuId) {
    TASK_REGISTRY.sweep_invalid(cpu_id);
}

/// Iterate the given CPU's diagnostic task registry.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64", feature = "watchdog"))]
#[inline]
pub(crate) fn for_each_tracked_task(cpu_id: LogicalCpuId, f: impl FnMut(&WeakKtaskRef)) {
    TASK_REGISTRY.for_each(cpu_id, f);
}
