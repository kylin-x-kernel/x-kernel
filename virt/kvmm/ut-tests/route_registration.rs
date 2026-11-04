// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise the production registration gate without touching the live route.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kiface_task_api::{ScheduleOutReason, TaskScheduleObserver};
use ksync::Mutex;
use lazyinit::LazyInit;

use super::prepare_route_once;

struct Fixture {
    ready: LazyInit<usize>,
    gate: Mutex<()>,
    has_entered: AtomicBool,
    is_released: AtomicBool,
    has_blocked: AtomicBool,
    attempts: AtomicUsize,
}

// SAFETY: the observer only publishes an atomic event. It neither waits nor
// takes locks, and does not change IRQ/preemption state or call the scheduler.
unsafe impl TaskScheduleObserver for Fixture {
    unsafe fn schedule_in(&self, _: kcpu_id_map::LogicalCpuId) {
        debug_assert!(!karch::local_irq_enabled());
    }

    unsafe fn schedule_out(&self, _: kcpu_id_map::LogicalCpuId, reason: ScheduleOutReason) {
        debug_assert!(!karch::local_irq_enabled());
        if reason == ScheduleOutReason::Blocked {
            // The controller observes actual mutex parking before releasing
            // the initializer; this is not a timing/yield-count assumption.
            self.has_blocked.store(true, Ordering::Release);
        }
    }
}

fn concurrent_registration(first_succeeds: bool) {
    let fixture = Arc::new(Fixture {
        ready: LazyInit::new(),
        gate: Mutex::new(()),
        has_entered: AtomicBool::new(false),
        is_released: AtomicBool::new(false),
        has_blocked: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
    });
    let first = fixture.clone();
    let owner = ktask::spawn_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let result = prepare_route_once(&first.ready, &first.gate, || {
                first.attempts.fetch_add(1, Ordering::Relaxed);
                // Publish entry while holding the production gate; the
                // controller releases us only after the contender parks.
                first.has_entered.store(true, Ordering::Release);
                while !first.is_released.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                first_succeeds.then_some(27)
            });
            core::assert_eq!(result, first_succeeds);
        },
        "route-register-owner".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    while !fixture.has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let second = fixture.clone();
    let contender = ktask::spawn_task(
        ktask::TaskInner::new_pidless_kthread(
            move || {
                core::assert!(prepare_route_once(&second.ready, &second.gate, || {
                    second.attempts.fetch_add(1, Ordering::Relaxed);
                    Some(27)
                }));
                core::assert_eq!(second.ready.get(), Some(&27));
            },
            "route-register-contender".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .with_schedule_observer(fixture.clone()),
    );
    while !fixture.has_blocked.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    core::assert!(fixture.ready.get().is_none());
    core::assert_eq!(fixture.attempts.load(Ordering::Relaxed), 1);
    fixture.is_released.store(true, Ordering::Release);
    core::assert_eq!(owner.join(), 0);
    core::assert_eq!(contender.join(), 0);
    core::assert_eq!(
        fixture.attempts.load(Ordering::Relaxed),
        if first_succeeds { 1 } else { 2 }
    );
    // Once published, later calls must not invoke initialization again.
    core::assert!(prepare_route_once(&fixture.ready, &fixture.gate, || {
        fixture.attempts.fetch_add(1, Ordering::Relaxed);
        None
    }));
    core::assert_eq!(
        fixture.attempts.load(Ordering::Relaxed),
        if first_succeeds { 1 } else { 2 }
    );
}

#[unittest::def_test]
fn concurrent_first_registration_waits_for_publication() {
    concurrent_registration(true);
}

#[unittest::def_test]
fn concurrent_registration_retries_after_failed_preparation() {
    concurrent_registration(false);
}
