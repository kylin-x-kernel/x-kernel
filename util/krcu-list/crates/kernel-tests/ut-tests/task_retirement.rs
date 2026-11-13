// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu_list::{Entry, RcuList};
use unittest::{assert, assert_eq, def_test};

struct Value {
    number: usize,
    probe: Option<Arc<DropProbe>>,
}

struct DropProbe {
    list: Weak<RcuList<Value>>,
    exclusion: Arc<kspin::SpinNoIrq<()>>,
    drops: AtomicUsize,
    has_valid_context: AtomicBool,
    has_reentered: AtomicBool,
}

impl Drop for Value {
    fn drop(&mut self) {
        let Some(probe) = self.probe.as_ref() else {
            return;
        };
        let has_valid_context = ktask::is_sleepable_context()
            && !krcu::is_reading()
            && karch::local_irq_enabled()
            && !kirq::context::is_in_interrupt_context();
        probe
            .has_valid_context
            .store(has_valid_context, Ordering::Release);
        probe.drops.fetch_add(1, Ordering::Release);
        if !has_valid_context {
            return;
        }
        let Some(list) = probe.list.upgrade() else {
            return;
        };
        let Ok(entry) = Entry::new_nowait(Value {
            number: 3,
            probe: None,
        }) else {
            return;
        };
        // Reacquiring consumer exclusion also diagnoses a destructor run under
        // its old guard. The external runner timeout catches a lock regression.
        let _guard = probe.exclusion.lock();
        // SAFETY: verified initialized task context; the old writer operation
        // finished before payload release. Allocation preceded both locks.
        unsafe { list.push_back(entry) };
        probe.has_reentered.store(true, Ordering::Release);
    }
}

#[def_test(serial)]
fn detached_successor_survives_reader_and_returns_task_owned_payload() {
    // Like concurrency.rs, the kernel harness supplies permanent RCU services
    // and boot-ready CPUs; this test neither starts nor stops those services.
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let old_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let list = Arc::new(RcuList::new());
    let exclusion = Arc::new(kspin::SpinNoIrq::new(()));
    let probe = Arc::new(DropProbe {
        list: Arc::downgrade(&list),
        exclusion: exclusion.clone(),
        drops: AtomicUsize::new(0),
        has_valid_context: AtomicBool::new(false),
        has_reentered: AtomicBool::new(false),
    });
    for number in 0..3 {
        let entry = Entry::new_nowait(Value {
            number,
            probe: (number == 1).then(|| probe.clone()),
        })
        .unwrap();
        // SAFETY: initialized task, no reader or reentrant writer; the entry
        // is fully allocated before publication takes the list lock.
        unsafe { list.push_back(entry) };
    }
    let has_entered = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let is_old_valid = Arc::new(AtomicBool::new(false));
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread_with_entry(
        ktask::PreparedTaskEntry::new({
            let list = list.clone();
            let probe = probe.clone();
            let has_entered = has_entered.clone();
            let can_exit = can_exit.clone();
            let is_old_valid = is_old_valid.clone();
            move || {
                krcu::with_read(|scope| {
                    let mut cursor = list.iter(scope);
                    let current = cursor.next();
                    // next() caches the successor before unlink can change the
                    // current node's link. Release/acquire orders this handshake.
                    has_entered.store(true, Ordering::Release);
                    while !can_exit.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    is_old_valid.store(
                        current.is_some_and(|value| value.number == 0)
                            && cursor.map(|value| value.number).eq([1, 2])
                            && probe.drops.load(Ordering::Acquire) == 0,
                        Ordering::Release,
                    );
                });
            }
        }).unwrap(),
        "rcu-list-task-retirement-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    while !has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let mut has_withdrawn = false;
    let mut is_new_valid = false;
    let mut has_observed_reader = false;
    let recovered = list.with_prepared_take(|prepared| {
        let retired = {
            let _guard = exclusion.lock();
            // SAFETY: preparing CPU0 task under consumer short exclusion;
            // predicate is bounded, nonblocking and does not reenter writers.
            unsafe { prepared.withdraw_first(|value| value.number == 1) }
        };
        has_withdrawn = retired.is_some();
        is_new_valid =
            krcu::with_read(|scope| list.iter(scope).map(|value| value.number).eq([0, 2]));
        // Unlink precedes release; never park waiting for a CPU1 reader whose
        // release event still depends on this task. No timing-based success.
        can_exit.store(true, Ordering::Release);
        // SAFETY: same preparing task, all short guards and read scopes ended;
        // permanent RCU services and CPUs remain available throughout the wait.
        let payload = unsafe { retired.reclaim() };
        has_observed_reader = is_old_valid.load(Ordering::Acquire);
        payload
    });
    // Preparation failure must also release and join the remote reader before
    // any assertion can return from this test.
    can_exit.store(true, Ordering::Release);
    let status = reader.join();
    drop(reader);
    let drops_before_release = probe.drops.load(Ordering::Acquire);
    let has_recovered = recovered
        .as_ref()
        .is_ok_and(|value| value.as_ref().is_some_and(|value| value.number == 1));
    drop(recovered);
    let is_reentry_visible =
        krcu::with_read(|scope| list.iter(scope).map(|value| value.number).eq([0, 2, 3]));
    let restored = ktask::set_current_affinity(old_affinity);
    assert_eq!(status, 0);
    assert!(restored && has_withdrawn && is_new_valid && has_recovered);
    assert!(has_observed_reader && is_old_valid.load(Ordering::Acquire));
    assert_eq!(drops_before_release, 0);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
    assert!(probe.has_valid_context.load(Ordering::Acquire));
    assert!(probe.has_reentered.load(Ordering::Acquire) && is_reentry_visible);
}
