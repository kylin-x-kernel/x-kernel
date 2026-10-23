// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real affinity migration of scoped readers and prepared writers.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

use crate::XArray;

#[derive(Default)]
struct History {
    has_entered: AtomicBool,
    has_migrated: AtomicBool,
    can_exit: AtomicBool,
    is_old_valid: AtomicBool,
    drops: [AtomicUsize; 2],
}

struct Version {
    number: usize,
    history: Arc<History>,
}

impl Drop for Version {
    fn drop(&mut self) {
        self.history.drops[self.number].fetch_add(1, Ordering::Release);
    }
}

// Progress is acknowledged by the task on its actual destination CPU, not by
// elapsed guest time. The bounded QEMU runner detects a stalled scheduler.
fn wait_for_flag(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        ktask::yield_now();
    }
}

fn prepare_reader(array: Arc<XArray<Box<Version>>>, history: Arc<History>) -> ktask::KtaskRef {
    ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            krcu::with_read(|scope| {
                let old = array.read(scope, usize::MAX);
                history.has_entered.store(true, Ordering::Release);
                // Keep the scope nonblocking; the remote coordinator releases it
                // after observing this task execute on the destination CPU.
                while !history.can_exit.load(Ordering::Acquire) {
                    if khal::percpu::this_cpu_id() == LogicalCpuId::new(2) {
                        history.has_migrated.store(true, Ordering::Release);
                    }
                    core::hint::spin_loop();
                }
                history.is_old_valid.store(
                    old.is_some_and(|value| value.number == 0)
                        && history.drops[0].load(Ordering::Acquire) == 0,
                    Ordering::Release,
                );
            });
        },
        "xarray-migrating-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ))
}

#[def_test(serial)]
fn migrated_reader_retains_old_owner_and_original_enrollment() {
    // CPU0 coordinates; the nonblocking reader moves CPU1 -> CPU2. Disjoint
    // affinity prevents it from starving the task that eventually releases it.
    if kcpu_id_map::nr_cpus() < 3 {
        return unittest::TestResult::Ignored;
    }
    let history = Arc::new(History::default());
    let array = Arc::new(XArray::<Box<Version>>::new());
    let old = Box::new(Version {
        number: 0,
        history: history.clone(),
    });
    // SAFETY: task context, atomic-only destructor and nonblocking Box/Arc free.
    assert!(unsafe { array.insert(usize::MAX, old) }.is_ok());
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let reader = prepare_reader(array.clone(), history.clone());
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    wait_for_flag(&history.has_entered);
    let new = Box::new(Version {
        number: 1,
        history: history.clone(),
    });
    // SAFETY: watched task, no scope/bookkeeping lock; permanent RCU service.
    let published = unsafe { array.set(usize::MAX, new) }.is_ok();
    let affinity_applied = published
        && ktask::set_task_affinity(
            &reader,
            ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(2)),
        );
    if affinity_applied {
        wait_for_flag(&history.has_migrated);
    }
    // SAFETY: this retained task owns reader storage; only atomic enrollment
    // observations are made from the coordinating CPU, never scope mutation.
    let reader_state = unsafe { reader.rcu_reader_for_integration() };
    let retains_source =
        reader_state.is_enrolled_on(krcu_core::RcuContextIf::reader_node_for_tests(1));
    let old_drops_before_exit = history.drops[0].load(Ordering::Acquire);
    let current = krcu::with_read(|scope| array.read(scope, usize::MAX).map(|v| v.number));
    history.can_exit.store(true, Ordering::Release);
    let status = reader.join();
    let is_unlinked = !reader_state.is_enrolled();
    drop(reader);
    drop(array);
    let is_affinity_restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: reader joined, captures released, no callback-required lock held.
    let drained = unsafe { krcu::drain_callbacks() };
    // Release/join and restore affinity before reporting any failed operation.
    assert!(published && affinity_applied && history.has_migrated.load(Ordering::Acquire));
    assert!(retains_source && is_unlinked);
    assert_eq!(old_drops_before_exit, 0);
    assert_eq!(current, Some(1));
    assert_eq!(status, 0);
    assert!(history.is_old_valid.load(Ordering::Acquire));
    assert!(is_affinity_restored && drained.is_ok());
    assert!(
        history
            .drops
            .iter()
            .all(|count| count.load(Ordering::Acquire) == 1)
    );
}

fn pause_prepared_writer(array: &XArray<Box<Version>>, index: usize) {
    let history = krcu::with_read(|scope| array.read(scope, index).map(|v| v.history.clone()));
    let Some(history) = history else { return };
    // No scope or writer lock spans the pause. Only the coordinating CPU asks
    // the scheduler to migrate us; this stack never explicitly yields.
    history.has_entered.store(true, Ordering::Release);
    while !history.can_exit.load(Ordering::Acquire) {
        if khal::percpu::this_cpu_id() == LogicalCpuId::new(2) {
            history.has_migrated.store(true, Ordering::Release);
        }
        core::hint::spin_loop();
    }
}

#[def_test(serial)]
fn prepared_writer_migrates_before_replacement_and_retires_once() {
    if kcpu_id_map::nr_cpus() < 3 {
        return unittest::TestResult::Ignored;
    }
    let history = Arc::new(History::default());
    let array = Arc::new(XArray::<Box<Version>>::new());
    assert!(
        // SAFETY: task context, atomic-only destructor and nonblocking Box/Arc free.
        unsafe {
            array.insert(
                usize::MAX,
                Box::new(Version {
                    number: 0,
                    history: history.clone(),
                }),
            )
        }
        .is_ok()
    );
    array.set_preparation_hook_for_test(pause_prepared_writer);
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let has_published = Arc::new(AtomicBool::new(false));
    let writer = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        {
            let array = array.clone();
            let history = history.clone();
            let has_published = has_published.clone();
            move || {
                let new = Box::new(Version { number: 1, history });
                // SAFETY: watched task, permanent RCU service, callback-safe owner.
                // The checkpoint leaves no scope/lock or altered context behind.
                has_published.store(
                    unsafe { array.set(usize::MAX, new) }.is_ok(),
                    Ordering::Release,
                );
            }
        },
        "xarray-migrating-writer".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    writer.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&writer);
    wait_for_flag(&history.has_entered);
    let affinity_applied = ktask::set_task_affinity(
        &writer,
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(2)),
    );
    if affinity_applied {
        wait_for_flag(&history.has_migrated);
    }
    let before = krcu::with_read(|scope| array.read(scope, usize::MAX).map(|v| v.number));
    let old_drops_before_commit = history.drops[0].load(Ordering::Acquire);
    history.can_exit.store(true, Ordering::Release);
    let status = writer.join();
    let after = krcu::with_read(|scope| array.read(scope, usize::MAX).map(|v| v.number));
    drop(writer);
    drop(array);
    let is_affinity_restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: writer joined, no scope or lock held; callbacks need no test progress.
    let drained = unsafe { krcu::drain_callbacks() };
    assert!(affinity_applied && history.has_migrated.load(Ordering::Acquire));
    assert!(has_published.load(Ordering::Acquire));
    assert_eq!(before, Some(0));
    assert_eq!(old_drops_before_commit, 0);
    assert_eq!(after, Some(1));
    assert_eq!(status, 0);
    assert!(is_affinity_restored && drained.is_ok());
    assert!(
        history
            .drops
            .iter()
            .all(|count| count.load(Ordering::Acquire) == 1)
    );
}
