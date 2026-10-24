// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in workload. Default UT skips it; the outer QEMU runner bounds hangs.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use krcu_list::{Entry, RcuList};
use unittest::{assert, assert_eq, def_test};

struct Value {
    id: usize,
    inverse: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for Value {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn opt_in_concurrent_publish_unlink_and_reclaim() {
    let Some(rounds) = option_env!("KRCU_LIST_STRESS_ROUNDS") else {
        return unittest::TestResult::Ignored;
    };
    let rounds: usize = rounds.parse().expect("positive stress round count");
    assert!(rounds > 0 && rounds <= 100_000);
    const WRITERS: usize = 4;
    let list = Arc::new(RcuList::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let is_invalid = Arc::new(AtomicBool::new(false));
    let can_start = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let list = list.clone();
        let drops = drops.clone();
        let completed = completed.clone();
        let is_invalid = is_invalid.clone();
        let can_start = can_start.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                while !can_start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                for index in 0..rounds {
                    let id = writer * rounds + index;
                    let entry = Entry::new(Value {
                        id,
                        inverse: !id,
                        drops: drops.clone(),
                    });
                    // SAFETY: task context, no reader/lock; prepare precedes mutation.
                    unsafe { list.push_back(entry) };
                    // SAFETY: watched sleepable task; Value's release is atomic-only
                    // plus nonblocking Arc/Box free. Predicate cannot block/reenter.
                    if unsafe { list.remove_first(|value| value.id == id) } != Ok(true) {
                        is_invalid.store(true, Ordering::Release);
                    }
                    if index % 16 == 0 {
                        ktask::yield_now();
                    }
                }
                completed.fetch_add(1, Ordering::Release);
            },
            "rcu-list-stress".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
        tasks.push(task);
    }
    let started = khal::time::monotonic_time().as_nanos_u64_saturating();
    can_start.store(true, Ordering::Release);
    let mut passes = 0usize;
    while completed.load(Ordering::Acquire) != WRITERS {
        let valid =
            krcu::with_read(|scope| list.iter(scope).all(|value| value.inverse == !value.id));
        if !valid {
            is_invalid.store(true, Ordering::Release);
        }
        passes += 1;
        ktask::yield_now();
    }
    let statuses: Vec<_> = tasks.into_iter().map(|task| task.join()).collect();
    let updated = khal::time::monotonic_time().as_nanos_u64_saturating();
    let remaining = krcu::with_read(|scope| list.iter(scope).count());
    drop(list);
    let before_drain = drops.load(Ordering::Acquire);
    // SAFETY: all workers joined, no scope/lock retained; callbacks are independent.
    let drained = unsafe { krcu::drain_callbacks() };
    let ended = khal::time::monotonic_time().as_nanos_u64_saturating();
    unittest::ktest_println!(
        "RCU_LIST_STRESS updates={} reader_passes={} update_ns={} drain_ns={} \
         pending_before_drain={}",
        WRITERS * rounds,
        passes,
        updated.saturating_sub(started),
        ended.saturating_sub(updated),
        (WRITERS * rounds).saturating_sub(before_drain),
    );
    assert!(drained.is_ok());
    assert!(statuses.iter().all(|status| *status == 0));
    assert!(!is_invalid.load(Ordering::Acquire));
    assert_eq!(remaining, 0);
    assert_eq!(drops.load(Ordering::Acquire), WRITERS * rounds);
}
