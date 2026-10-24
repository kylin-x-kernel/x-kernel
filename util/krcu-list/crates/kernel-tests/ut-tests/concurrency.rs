// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu_list::{Entry, RcuList};
use unittest::{assert, assert_eq, def_test};

const ENTRIES: usize = 128;

struct Value {
    number: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

#[def_test(serial)]
fn old_reader_retains_entire_removed_chain_then_backlog_drains() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let old_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let list = Arc::new(RcuList::new());
    let drops = Arc::new(AtomicUsize::new(0));
    for number in 0..ENTRIES {
        // SAFETY: initialized task, allocation completed before writer locking.
        unsafe {
            list.push_back(Entry::new(Value {
                number,
                drops: drops.clone(),
            }))
        };
    }
    let has_entered = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let is_old_valid = Arc::new(AtomicBool::new(false));
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        {
            let list = list.clone();
            let has_entered = has_entered.clone();
            let can_exit = can_exit.clone();
            let is_old_valid = is_old_valid.clone();
            move || {
                krcu::with_read(|scope| {
                    let mut cursor = list.iter(scope);
                    let first = cursor.next();
                    has_entered.store(true, Ordering::Release);
                    // No voluntary scheduling inside a scope. CPU0 owns the release
                    // event and cannot be starved by this CPU1-bound reader.
                    while !can_exit.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    is_old_valid.store(
                        first.is_some_and(|value| {
                            value.number == 0 && value.drops.load(Ordering::Acquire) == 0
                        }) && cursor.map(|value| value.number).eq(1..ENTRIES),
                        Ordering::Release,
                    );
                });
            }
        },
        "rcu-list-old-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    while !has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let mut removed = 0;
    for number in 0..ENTRIES {
        // SAFETY: CPU0 is a watched sleepable task, holds no scope/lock; Value
        // only increments an atomic and releases a nonblocking Arc allocation.
        if unsafe { list.remove_first(|value| value.number == number) } == Ok(true) {
            removed += 1;
        }
    }
    let new_count = krcu::with_read(|scope| list.iter(scope).count());
    let drops_before_exit = drops.load(Ordering::Acquire);
    // Release and join before assertions, including on an unexpected removal error.
    can_exit.store(true, Ordering::Release);
    let status = reader.join();
    drop(reader);
    drop(list);
    let restored = ktask::set_current_affinity(old_affinity);
    // SAFETY: reader exited; no retained scope or callback-required lock.
    let drained = unsafe { krcu::drain_callbacks() };
    assert_eq!(status, 0);
    assert!(restored && drained.is_ok());
    assert_eq!(removed, ENTRIES);
    assert_eq!(new_count, 0);
    assert_eq!(drops_before_exit, 0);
    assert!(is_old_valid.load(Ordering::Acquire));
    assert_eq!(drops.load(Ordering::Acquire), ENTRIES);
}
