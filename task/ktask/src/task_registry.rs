// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Diagnostic-owned registration and reader-safe retirement of task weak refs.

use core::sync::atomic::{AtomicPtr, Ordering};

use kcpu_id_map::LogicalCpuId;
use khal::percpu::this_cpu_id;

use crate::{KtaskRef, WeakKtaskRef};

mod slot;
use slot::Slot;

const TASK_REGISTRY_SLOTS: usize = 4096;

struct TaskRegistry {
    slots: [[Slot<WeakKtaskRef>; TASK_REGISTRY_SLOTS]; kbuild_config::NR_CPUS],
}

impl TaskRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { [const { Slot::new() }; TASK_REGISTRY_SLOTS] }; kbuild_config::NR_CPUS],
        }
    }

    fn try_insert(
        &'static self,
        cpu_id: LogicalCpuId,
        mut weak: WeakKtaskRef,
    ) -> Option<&'static Slot<WeakKtaskRef>> {
        for slot in &self.slots[cpu_id.as_usize()] {
            match slot.try_insert(weak) {
                Ok(()) => return Some(slot),
                Err(returned) => weak = returned,
            }
        }
        None
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64", feature = "watchdog"))]
    fn for_each(&self, cpu_id: LogicalCpuId, mut visit: impl FnMut(KtaskRef)) {
        for slot in &self.slots[cpu_id.as_usize()] {
            let task = slot.read().and_then(|weak| weak.upgrade());
            if let Some(task) = task {
                visit(task);
            }
        }
    }
}

static TASK_REGISTRY: TaskRegistry = TaskRegistry::new();

/// A task's registration location, independent of its current CPU.
///
/// Retirement is idempotent and closes diagnostic discovery without waiting
/// for readers. The final counted reader releases the inline Weak; no reader
/// owns a separately allocated registry box. NMI readers therefore require the
/// local irq_work readiness used by DeferredWeak's final release.
pub(crate) struct Registration {
    slot: AtomicPtr<Slot<WeakKtaskRef>>,
}

impl Registration {
    pub(crate) const fn new() -> Self {
        Self {
            slot: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    fn attach(&self, slot: &'static Slot<WeakKtaskRef>) {
        if self
            .slot
            .compare_exchange(
                core::ptr::null_mut(),
                core::ptr::from_ref(slot).cast_mut(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_err()
        {
            slot.retire();
        }
    }

    pub(crate) fn retire(&self) {
        let slot = self.slot.swap(core::ptr::null_mut(), Ordering::AcqRel);
        if !slot.is_null() {
            // SAFETY: attach accepts only permanent registry slots. The swap
            // consumes this generation's only registration before allowing
            // its slot to be retired and reused, including concurrent calls.
            unsafe { &*slot }.retire();
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.retire();
    }
}

/// Register once before a task becomes runnable, or during boot publication.
///
/// Creation and retirement must not overlap. Repeated registration of an
/// already registered task is ignored; normal CPU migration never re-registers.
pub(crate) fn record_tracked_task(task: &KtaskRef) {
    if !task
        .diagnostic_registration
        .slot
        .load(Ordering::Acquire)
        .is_null()
    {
        return;
    }
    let cpu = this_cpu_id();
    if let Some(slot) = TASK_REGISTRY.try_insert(cpu, KtaskRef::downgrade(task)) {
        task.diagnostic_registration.attach(slot);
    } else {
        warn!("task registry on cpu {} is full!", cpu.as_usize());
    }
}

/// Visit acquired task owners, with no registry borrow held during callbacks.
///
/// Readers that acquired an owner before retirement may finish afterwards.
/// Publishing/retiring slots are skipped, never waited upon from NMI.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64", feature = "watchdog"))]
pub(crate) fn for_each_tracked_task(cpu_id: LogicalCpuId, visit: impl FnMut(KtaskRef)) {
    TASK_REGISTRY.for_each(cpu_id, visit);
}

unittest::path_mod_test!(tests, "../ut-tests/task_registry.rs");
