// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One old reader spans both withdrawals and their later RCU submissions.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kalloc_types::AllocFlags;
use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kxarray::XArray;
use unittest::{assert, assert_eq, def_test};

struct Value(Arc<AtomicUsize>);

impl Drop for Value {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[def_test(serial)]
fn paired_retirement_preserves_one_shared_owner_until_reader_exit() {
    // A spinning reader must not depend on time-slice preemption for progress.
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let arrays = Arc::new([XArray::<Arc<Value>>::new(), XArray::new()]);
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Value(drops.clone()));
    for array in arrays.iter() {
        assert!(
            // SAFETY: task context; Value only increments an atomic and frees its
            // independent Arc through the nonblocking allocator.
            unsafe { array.insert_with_flags(usize::MAX, owner.clone(), AllocFlags::NOWAIT) }
                .is_ok()
        );
    }
    drop(owner);
    // SAFETY: private NOWAIT allocation in initialized task context.
    let first = unsafe { arrays[0].prepare_remove(AllocFlags::NOWAIT) }.unwrap();
    // SAFETY: same task and allocation context.
    let second = unsafe { arrays[1].prepare_remove(AllocFlags::NOWAIT) }.unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let is_valid = Arc::new(AtomicBool::new(false));
    let reader_arrays = arrays.clone();
    let reader_entered = entered.clone();
    let reader_exit = can_exit.clone();
    let reader_valid = is_valid.clone();
    let original_affinity = ktask::current().cpumask();
    assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0))
    ));
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread_with_entry(
        ktask::PreparedTaskEntry::new(move || {
            krcu::with_read(|scope| {
                let left = reader_arrays[0].read(scope, usize::MAX);
                let right = reader_arrays[1].read(scope, usize::MAX);
                reader_entered.store(true, Ordering::Release);
                while !reader_exit.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                reader_valid.store(
                    left.zip(right).is_some_and(|(left, right)| {
                        core::ptr::eq(left, right) && left.0.load(Ordering::Acquire) == 0
                    }),
                    Ordering::Release,
                );
            });
        }).unwrap(),
        "xarray-paired-retirement".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1)));
    ktask::activate_task(&reader);
    while !entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let exclusion = kspin::SpinNoIrq::new(());
    let admitted = krcu::preflight_callback_submission().is_ok();
    let mut removed_both = false;
    if admitted {
        let pending = {
            let _guard = exclusion.lock();
            // SAFETY: successful task preflight precedes this temporary guard;
            // the test owns both writer paths and submits only after unlocking.
            let left = unsafe { arrays[0].withdraw_prepared(usize::MAX, first) };
            // SAFETY: same admitted transaction; no callback submission here.
            let right = unsafe { arrays[1].withdraw_prepared(usize::MAX, second) };
            [left, right]
        };
        removed_both = pending.iter().all(Option::is_some);
        for pending in pending.into_iter().flatten() {
            // SAFETY: caller-held exclusion has ended and no context change or
            // read scope intervenes; both withdrawn owners are callback-safe.
            unsafe { pending.retire() };
        }
    }
    let before_exit = drops.load(Ordering::Acquire);
    let absent = krcu::with_read(|scope| {
        arrays
            .iter()
            .all(|array| array.read(scope, usize::MAX).is_none())
    });
    can_exit.store(true, Ordering::Release);
    let status = reader.join();
    drop(reader);
    drop(arrays);
    let restored = ktask::set_current_affinity(original_affinity);
    // SAFETY: reader exited and callbacks need no resources held by this task.
    let drained = unsafe { krcu::drain_callbacks() };
    // Never strand the spinning reader when reporting a failed observation.
    assert!(admitted && removed_both && absent && restored);
    assert_eq!(before_exit, 0);
    assert_eq!(status, 0);
    assert!(is_valid.load(Ordering::Acquire));
    assert!(drained.is_ok());
    assert_eq!(drops.load(Ordering::Acquire), 1);
}
