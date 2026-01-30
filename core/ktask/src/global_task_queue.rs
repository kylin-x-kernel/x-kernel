//! Lock-free per-CPU task registry for watchdog and NMI dumping.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::percpu::this_cpu_id;

use crate::WeakKtaskRef;

/// Max number of task weak refs tracked per CPU for watchdog/NMI dumping.
///
/// This is a best-effort debug facility: if it fills up, we will simply drop
/// new records (and dumps may miss some tasks).
const GLOBAL_TASK_QUEUE_SLOTS: usize = 4096;

/// Lock-free per-CPU task registry for watchdog/NMI dumping.
///
/// Safety / design notes:
/// - Writers (task creation + GC) run on the owning CPU, but NMI may read any CPU.
/// - Each slot stores a raw pointer to a heap-allocated `WeakKtaskRef` (usize).
/// - Readers snapshot the pointers with `Acquire` loads, dereference, and call
///   `upgrade()` (which is internally atomic).
/// - GC sweeps invalid weak refs and frees their boxes.
struct GlobalTaskRegistry {
    slots: [[AtomicUsize; GLOBAL_TASK_QUEUE_SLOTS]; platconfig::plat::CPU_NUM],
}

impl GlobalTaskRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { [const { AtomicUsize::new(0) }; GLOBAL_TASK_QUEUE_SLOTS] };
                platconfig::plat::CPU_NUM],
        }
    }

    #[inline]
    fn try_insert(&self, cpu_id: usize, weak: WeakKtaskRef) {
        // Allocate once; if we fail to insert, free it immediately.
        let boxed = Box::new(weak);
        let ptr = Box::into_raw(boxed) as usize;

        for slot in &self.slots[cpu_id] {
            if slot
                .compare_exchange(0, ptr, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }

        warn!("global task queue on cpu {} is full!", cpu_id);

        // registry full, drop record
        unsafe { drop(Box::from_raw(ptr as *mut WeakKtaskRef)) };
    }

    #[inline]
    fn sweep_invalid(&self, cpu_id: usize) {
        for slot in &self.slots[cpu_id] {
            let ptr = slot.load(Ordering::Acquire);
            if ptr == 0 {
                continue;
            }
            // Safety: ptr is either 0 or a valid Box<WeakKtaskRef> installed by try_insert.
            let weak = unsafe { &*(ptr as *const WeakKtaskRef) };
            if weak.upgrade().is_none() {
                // Try to claim the slot and free.
                if slot
                    .compare_exchange(ptr, 0, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    unsafe { drop(Box::from_raw(ptr as *mut WeakKtaskRef)) };
                }
            }
        }
    }

    #[inline]
    fn for_each(&self, cpu_id: usize, mut f: impl FnMut(&WeakKtaskRef)) {
        for slot in &self.slots[cpu_id] {
            let ptr = slot.load(Ordering::Acquire);
            if ptr == 0 {
                continue;
            }
            // Safety: ptr is either 0 or a valid Box<WeakKtaskRef>.
            let weak = unsafe { &*(ptr as *const WeakKtaskRef) };
            f(weak);
        }
    }
}

static GLOBAL_TASK_REGISTRY: GlobalTaskRegistry = GlobalTaskRegistry::new();

/// Record a task into the current CPU's watchdog registry.
#[inline]
pub(crate) fn record_task_for_watchdog(task: &crate::KtaskRef) {
    GLOBAL_TASK_REGISTRY.try_insert(this_cpu_id(), Arc::downgrade(task));
}

/// Sweep invalid weak refs from the current CPU's watchdog registry.
#[inline]
pub(crate) fn sweep_watchdog_tasks(cpu_id: usize) {
    GLOBAL_TASK_REGISTRY.sweep_invalid(cpu_id);
}

/// Iterate the given CPU's watchdog registry.
#[inline]
pub(crate) fn for_each_watchdog_task(cpu_id: usize, f: impl FnMut(&WeakKtaskRef)) {
    GLOBAL_TASK_REGISTRY.for_each(cpu_id, f);
}
