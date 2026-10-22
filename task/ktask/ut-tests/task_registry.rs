// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Diagnostic registration ownership and retirement regressions.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{Registration, TaskRegistry, slot::Slot};

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[def_test]
fn registry_slot_waits_for_all_readers_without_blocking_retirement() {
    let drops = Arc::new(AtomicUsize::new(0));
    let slot = Slot::new();
    assert!(slot.try_insert(DropCount(drops.clone())).is_ok());
    let first = slot.read().unwrap();
    let second = slot.read().unwrap();
    slot.retire();
    assert!(slot.read().is_none());
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(first);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(second);
    assert_eq!(drops.load(Ordering::Acquire), 1);
    assert!(slot.try_insert(DropCount(drops.clone())).is_ok());
    slot.retire();
    assert_eq!(drops.load(Ordering::Acquire), 2);
}

#[def_test]
fn registry_registration_retires_once_and_does_not_remove_replacement() {
    static SLOT: Slot<crate::WeakKtaskRef> = Slot::new();
    let slot = &SLOT;
    let task = crate::current().clone();
    assert!(slot.try_insert(crate::KtaskRef::downgrade(&task)).is_ok());
    let registration = Registration::new();
    registration.attach(slot);
    let reader = slot.read().unwrap();
    registration.retire();
    assert!(slot.read().is_none());
    assert!(reader.upgrade().is_some());
    drop(reader);
    assert!(slot.try_insert(crate::KtaskRef::downgrade(&task)).is_ok());
    registration.retire();
    drop(registration);
    assert!(slot.read().is_some());
    slot.retire();
}

#[def_test]
fn registry_registration_drop_is_cleanup_fallback() {
    static SLOT: Slot<crate::WeakKtaskRef> = Slot::new();
    let slot = &SLOT;
    let task = crate::current().clone();
    assert!(slot.try_insert(crate::KtaskRef::downgrade(&task)).is_ok());
    let registration = Registration::new();
    registration.attach(slot);
    drop(registration);
    assert!(slot.read().is_none());
    assert!(slot.try_insert(crate::KtaskRef::downgrade(&task)).is_ok());
    slot.retire();
}

#[def_test]
fn registry_live_task_exit_retires_its_original_cpu_slot() {
    let release = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let worker_release = release.clone();
    let exited_cpu = Arc::new(AtomicUsize::new(usize::MAX));
    let worker_cpu = exited_cpu.clone();
    let guard = kspin::NoPreempt::new();
    let registered_cpu = khal::percpu::this_cpu_id().as_usize();
    let task = crate::spawn(move || {
        while !worker_release.load(Ordering::Acquire) {
            crate::yield_now();
        }
        worker_cpu.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
    });
    drop(guard);
    let slot = task.diagnostic_registration.slot.load(Ordering::Acquire);
    let target_cpu = if cfg!(feature = "smp") {
        (registered_cpu + 1) % kcpu_id_map::nr_cpus()
    } else {
        registered_cpu
    };
    let mut mask = crate::KCpuMask::new();
    mask.set(target_cpu, true);
    let changed = crate::set_task_affinity(&task, mask);
    release.store(true, Ordering::Release);
    assert_eq!(task.join(), 0);
    #[cfg(feature = "smp")]
    while task.on_cpu() {
        crate::yield_now();
    }
    while !task.diagnostic_registration.slot.load(Ordering::Acquire).is_null() {
        crate::yield_now();
    }
    assert!(changed);
    assert_eq!(exited_cpu.load(Ordering::Acquire), target_cpu);
    assert!(!slot.is_null());
    // SAFETY: Registration only stores pointers into the permanent registry;
    // retirement may reuse its contents, but never invalidates the slot itself.
    let slot = unsafe { &*slot };
    let observed = slot.read().and_then(|weak| weak.upgrade());
    assert!(observed.is_none_or(|other| !crate::KtaskRef::ptr_eq(&other, &task)));
    let weak = crate::KtaskRef::downgrade(&task);
    assert!(weak.upgrade().is_some());
}

#[def_test]
fn registry_layout_cost_matches_inline_storage() {
    assert_eq!(core::mem::size_of::<Registration>(), core::mem::size_of::<usize>());
    assert_eq!(core::mem::size_of::<Slot<crate::WeakKtaskRef>>(), 4 * core::mem::size_of::<usize>());
    assert_eq!(core::mem::size_of::<TaskRegistry>(), kbuild_config::NR_CPUS * 4096 * 4 * core::mem::size_of::<usize>());
}
