// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-backed container tests use the permanently initialized boot RCU host.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use crate::XArray;

#[def_test]
fn competing_insertions_keep_one_owner_and_return_every_loser() {
    const WRITERS: usize = 4;
    let array = Arc::new(XArray::<Arc<usize>>::new());
    let ready = Arc::new(AtomicUsize::new(0));
    let can_start = Arc::new(AtomicBool::new(false));
    let winners = Arc::new(AtomicUsize::new(0));
    let has_invalid_return = Arc::new(AtomicBool::new(false));
    let owners: Vec<_> = (0..WRITERS).map(Arc::new).collect();
    let mut tasks = Vec::new();
    for owner in &owners {
        let offered = owner.clone();
        let array = array.clone();
        let ready = ready.clone();
        let can_start = can_start.clone();
        let winners = winners.clone();
        let has_invalid_return = has_invalid_return.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                let original = Arc::as_ptr(&offered);
                ready.fetch_add(1, Ordering::Release);
                while !can_start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                // SAFETY: task context, primitive payload and nonblocking Arc
                // deallocation; no exclusion guard or read scope is held.
                match unsafe { array.insert(usize::MAX, offered) } {
                    Ok(()) => {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(returned) => {
                        if Arc::as_ptr(&returned) != original {
                            has_invalid_return.store(true, Ordering::Relaxed);
                        }
                    }
                }
            },
            "xarray-insert".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
        tasks.push(task);
    }
    while ready.load(Ordering::Acquire) != WRITERS {
        ktask::yield_now();
    }
    can_start.store(true, Ordering::Release);
    let statuses: Vec<_> = tasks.iter().map(|task| task.join()).collect();
    // task_entry consumes each FnOnce before publishing exit. Retained task
    // handles must not retain the closures' captured array owners.
    assert_eq!(Arc::strong_count(&array), 1);
    let winner = krcu::with_read(|scope| array.read(scope, usize::MAX).copied());
    assert!(statuses.iter().all(|status| *status == 0));
    assert_eq!(winners.load(Ordering::Relaxed), 1);
    assert!(!has_invalid_return.load(Ordering::Relaxed));
    assert!(winner.is_some());
    for (index, owner) in owners.iter().enumerate() {
        assert_eq!(
            Arc::strong_count(owner),
            if winner == Some(index) { 2 } else { 1 }
        );
    }
    drop(array);
    assert!(owners.iter().all(|owner| Arc::strong_count(owner) == 1));
}

#[def_test]
fn competing_sets_release_every_displaced_owner_once() {
    const WRITERS: usize = 4;
    struct Value {
        number: usize,
        drops: Arc<[AtomicUsize; WRITERS]>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops[self.number].fetch_add(1, Ordering::Release);
        }
    }
    for key in [0, usize::MAX] {
        let array = Arc::new(XArray::<Box<Value>>::new());
        let ready = Arc::new(AtomicUsize::new(0));
        let can_start = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(core::array::from_fn(|_| AtomicUsize::new(0)));
        let mut tasks = Vec::new();
        for number in 0..WRITERS {
            let value = Box::new(Value {
                number,
                drops: drops.clone(),
            });
            let array = array.clone();
            let ready = ready.clone();
            let can_start = can_start.clone();
            let accepted = accepted.clone();
            let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                move || {
                    ready.fetch_add(1, Ordering::Release);
                    while !can_start.load(Ordering::Acquire) {
                        ktask::yield_now();
                    }
                    // SAFETY: watched task context, no scopes or locks held.
                    // The payload's release is atomic-only plus nonblocking
                    // allocation release; callbacks retain their own counters.
                    if unsafe { array.set(key, value) }.is_ok() {
                        accepted.fetch_add(1, Ordering::Release);
                    }
                },
                "xarray-set".into(),
                kbuild_config::TASK_STACK_SIZE,
            ));
            ktask::activate_task(&task);
            tasks.push(task);
        }
        while ready.load(Ordering::Acquire) != WRITERS {
            ktask::yield_now();
        }
        can_start.store(true, Ordering::Release);
        let statuses: Vec<_> = tasks.iter().map(|task| task.join()).collect();
        drop(tasks);
        let winner = krcu::with_read(|scope| array.read(scope, key).map(|value| value.number));
        // SAFETY: writers have returned and no read scope or callback-required
        // resource is held. The permanent service can finish every retirement.
        let drained = unsafe { krcu::drain_callbacks() };
        let retired_counts =
            core::array::from_fn::<_, WRITERS, _>(|i| drops[i].load(Ordering::Acquire));
        drop(array);
        assert!(statuses.iter().all(|status| *status == 0));
        assert_eq!(accepted.load(Ordering::Acquire), WRITERS);
        assert!(drained.is_ok());
        assert!(winner.is_some());
        for (number, count) in retired_counts.into_iter().enumerate() {
            assert_eq!(count, usize::from(winner != Some(number)));
            assert_eq!(drops[number].load(Ordering::Acquire), 1);
        }
    }
}

#[def_test]
fn published_entries_remain_readable_during_root_growth() {
    let keys = Arc::new(
        core::iter::once(0)
            .chain((6..usize::BITS).step_by(6).map(|bits| 1_usize << bits))
            .chain(core::iter::once(usize::MAX))
            .collect::<Vec<_>>(),
    );
    let array = Arc::new(XArray::<Box<usize>>::new());
    let published = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let has_failed_insert = Arc::new(AtomicBool::new(false));
    let writer_array = array.clone();
    let writer_keys = keys.clone();
    let writer_published = published.clone();
    let writer_observed = observed.clone();
    let writer_failure = has_failed_insert.clone();
    let writer = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            for (position, key) in writer_keys.iter().copied().enumerate() {
                // SAFETY: task context, no held guard, primitive Box release.
                if unsafe { writer_array.insert(key, Box::new(key)) }.is_err() {
                    writer_failure.store(true, Ordering::Relaxed);
                }
                writer_published.store(position + 1, Ordering::Release);
                while writer_observed.load(Ordering::Acquire) < position + 1 {
                    ktask::yield_now();
                }
            }
        },
        "xarray-grow".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    ktask::activate_task(&writer);
    let mut is_consistent = true;
    while observed.load(Ordering::Relaxed) < keys.len() {
        let count = published.load(Ordering::Acquire);
        is_consistent &= krcu::with_read(|scope| {
            keys[..count]
                .iter()
                .all(|key| array.read(scope, *key) == Some(key))
        });
        observed.store(count, Ordering::Release);
        // Never block within a scope. The acknowledgement lets the writer
        // extend the tree while subsequent reads revisit the published prefix.
        ktask::yield_now();
    }
    let status = writer.join();
    assert_eq!(status, 0);
    assert!(!has_failed_insert.load(Ordering::Relaxed));
    assert!(is_consistent);
}

#[def_test]
fn competing_removals_withdraw_one_owner() {
    const REMOVERS: usize = 4;
    let owner = Arc::new(7_usize);
    let array = Arc::new(XArray::<Arc<usize>>::new());
    // SAFETY: initialized task, primitive Arc payload and nonblocking release.
    assert!(unsafe { array.insert(usize::MAX, owner.clone()) }.is_ok());
    let ready = Arc::new(AtomicUsize::new(0));
    let can_start = Arc::new(AtomicBool::new(false));
    let removed = Arc::new(AtomicUsize::new(0));
    let absent = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..REMOVERS {
        let array = array.clone();
        let ready = ready.clone();
        let can_start = can_start.clone();
        let removed = removed.clone();
        let absent = absent.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                ready.fetch_add(1, Ordering::Release);
                while !can_start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                // SAFETY: watched sleepable task, no scope or bookkeeping lock;
                // the permanent RCU service remains available for retirement.
                match unsafe { array.remove(usize::MAX) } {
                    Ok(true) => {
                        removed.fetch_add(1, Ordering::Release);
                    }
                    Ok(false) => {
                        absent.fetch_add(1, Ordering::Release);
                    }
                    Err(_) => {}
                }
            },
            "xarray-remove".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
        tasks.push(task);
    }
    while ready.load(Ordering::Acquire) != REMOVERS {
        ktask::yield_now();
    }
    can_start.store(true, Ordering::Release);
    let statuses: Vec<_> = tasks.iter().map(|task| task.join()).collect();
    drop(tasks);
    let is_absent = krcu::with_read(|scope| array.read(scope, usize::MAX).is_none());
    drop(array);
    // SAFETY: all producers joined, no read scope or callback-required lock held.
    let drained = unsafe { krcu::drain_callbacks() };
    assert!(statuses.iter().all(|status| *status == 0));
    assert_eq!(removed.load(Ordering::Acquire), 1);
    assert_eq!(absent.load(Ordering::Acquire), REMOVERS - 1);
    assert!(is_absent);
    assert!(drained.is_ok());
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn removal_and_reinsertion_preserve_a_held_old_borrow() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    // The reader spins without blocking in its scope; another CPU must remain
    // available for the writer and RCU service even without timer preemption.
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    struct Version {
        number: usize,
        drops: Arc<[AtomicUsize; 2]>,
    }
    impl Drop for Version {
        fn drop(&mut self) {
            self.drops[self.number].fetch_add(1, Ordering::Release);
        }
    }
    let array = Arc::new(XArray::<Box<Version>>::new());
    let drops = Arc::new(core::array::from_fn(|_| AtomicUsize::new(0)));
    // MAX builds the deepest path, all of which becomes empty on withdrawal.
    let old = Box::new(Version {
        number: 0,
        drops: drops.clone(),
    });
    // SAFETY: task context, atomic-only destructor and nonblocking Box/Arc free.
    assert!(unsafe { array.insert(usize::MAX, old) }.is_ok());
    let has_entered = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let is_valid = Arc::new(AtomicBool::new(false));
    let reader_array = array.clone();
    let reader_entered = has_entered.clone();
    let reader_exit = can_exit.clone();
    let reader_valid = is_valid.clone();
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            krcu::with_read(|scope| {
                let borrowed = reader_array.read(scope, usize::MAX);
                reader_entered.store(true, Ordering::Release);
                while !reader_exit.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                reader_valid.store(
                    borrowed.is_some_and(|value| {
                        value.number == 0 && value.drops[0].load(Ordering::Acquire) == 0
                    }),
                    Ordering::Release,
                );
            });
        },
        "xarray-held-removal".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    while !has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    // SAFETY: writer is outside all read scopes and holds no bookkeeping lock.
    let removed = unsafe { array.remove(usize::MAX) };
    let is_absent = krcu::with_read(|scope| array.read(scope, usize::MAX).is_none());
    let new = Box::new(Version {
        number: 1,
        drops: drops.clone(),
    });
    // SAFETY: same task context and callback-safe release as initial insertion.
    let inserted = unsafe { array.insert(usize::MAX, new) }.is_ok();
    let current = krcu::with_read(|scope| array.read(scope, usize::MAX).map(|value| value.number));
    let before_exit = drops[0].load(Ordering::Acquire);
    can_exit.store(true, Ordering::Release);
    let status = reader.join();
    drop(reader);
    drop(array);
    let is_affinity_restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: the reader exited; callbacks own their counters independently and
    // need no resource held by this test. Array lifetime is no longer required.
    let drained = unsafe { krcu::drain_callbacks() };
    // Cleanup and join precede assertions so failure cannot strand the reader.
    assert_eq!(removed, Ok(true));
    assert!(is_absent);
    assert!(inserted);
    assert_eq!(current, Some(1));
    assert_eq!(before_exit, 0);
    assert_eq!(status, 0);
    assert!(is_affinity_restored);
    assert!(is_valid.load(Ordering::Acquire));
    assert!(drained.is_ok());
    assert!(drops.iter().all(|count| count.load(Ordering::Acquire) == 1));
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn successive_replacements_preserve_two_held_versions() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    // Each spinning reader gets a separate CPU; the test and GP service retain
    // another CPU for progress. No reader voluntarily blocks inside its scope.
    if kcpu_id_map::nr_cpus() < 3 {
        return unittest::TestResult::Ignored;
    }
    struct History {
        has_entered: [AtomicBool; 2],
        can_exit: [AtomicBool; 2],
        is_valid: [AtomicBool; 2],
        drops: [AtomicUsize; 3],
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
    let history = Arc::new(History {
        has_entered: core::array::from_fn(|_| AtomicBool::new(false)),
        can_exit: core::array::from_fn(|_| AtomicBool::new(false)),
        is_valid: core::array::from_fn(|_| AtomicBool::new(false)),
        drops: core::array::from_fn(|_| AtomicUsize::new(0)),
    });
    let array = Arc::new(XArray::<Box<Version>>::new());
    let mut readers = Vec::new();
    let mut did_publish = true;
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    for number in 0..3 {
        let value = Box::new(Version {
            number,
            history: history.clone(),
        });
        // SAFETY: watched task outside read scopes/locks. Callback destruction
        // only updates an atomic and releases independently owned allocations.
        did_publish &= unsafe { array.set(64, value) }.is_ok();
        if number == 2 {
            break;
        }
        let reader_array = array.clone();
        let reader_history = history.clone();
        let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                krcu::with_read(|scope| {
                    let borrowed = reader_array.read(scope, 64);
                    reader_history.has_entered[number].store(true, Ordering::Release);
                    while !reader_history.can_exit[number].load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    reader_history.is_valid[number].store(
                        borrowed.is_some_and(|value| value.number == number)
                            && reader_history.drops[number].load(Ordering::Acquire) == 0,
                        Ordering::Release,
                    );
                });
            },
            "xarray-held-version".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(
            number + 1,
        )));
        ktask::activate_task(&reader);
        readers.push(reader);
        while !history.has_entered[number].load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
    let current = krcu::with_read(|scope| array.read(scope, 64).map(|value| value.number));
    let before_exit = core::array::from_fn::<_, 3, _>(|i| history.drops[i].load(Ordering::Acquire));
    history.can_exit[0].store(true, Ordering::Release);
    let first_status = readers[0].join();
    let second_still_live = history.drops[1].load(Ordering::Acquire) == 0;
    history.can_exit[1].store(true, Ordering::Release);
    let second_status = readers[1].join();
    drop(readers);
    drop(array);
    let is_affinity_restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: readers have exited and no resource required by callbacks is
    // held. The boot RCU service remains available through completion.
    let drained = unsafe { krcu::drain_callbacks() };
    // Report only after releasing and joining both readers, even on failure.
    assert!(did_publish);
    assert!(is_affinity_restored);
    assert_eq!(current, Some(2));
    assert_eq!(before_exit, [0, 0, 0]);
    assert_eq!([first_status, second_status], [0, 0]);
    assert!(second_still_live);
    assert!(drained.is_ok());
    assert!(
        history
            .is_valid
            .iter()
            .all(|value| value.load(Ordering::Acquire))
    );
    assert!(
        history
            .drops
            .iter()
            .all(|value| value.load(Ordering::Acquire) == 1)
    );
}
