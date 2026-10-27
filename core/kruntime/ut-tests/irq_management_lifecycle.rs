// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real task registration/free serialization with synthetic in-flight accounting.
//!
//! No handler runs, controller claim is fabricated, or hardware source enabled.
//! The permanent boot task/IRQ services supply the real mutex and drain waits.
//! Both UP and SMP are supported: all contenders may park and the coordinator
//! yields. An external QEMU timeout diagnoses a stalled handshake; elapsed time
//! is never a pass criterion. Test-owned actions are withdrawn before assertions.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::Completion;
use kirq::{IrqEvent, IrqHandler, tests_irq::RetainedInFlight};
use ktask::TaskState;
use unittest::{assert, assert_eq, def_test};

struct HandlerProbe {
    drops: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

impl IrqHandler for HandlerProbe {
    fn handle(&self, _irq: usize) -> IrqEvent {
        self.calls.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }
}

impl Drop for HandlerProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Release);
    }
}

#[def_test(serial)]
fn masked_registration_rejects_normal_admission_but_allows_explicit_startup() {
    let virq = 0x5f72;
    assert!(kirq::descriptor(virq).is_none());
    let handler: Arc<dyn IrqHandler> = Arc::new(|_| IrqEvent::HANDLED);
    let (ordinary, was_absent, startup, stayed_masked) = {
        let _irq = kspin::IrqSave::new();
        let ordinary = kirq::try_register_disabled(virq, handler.clone());
        let was_absent = kirq::descriptor(virq).is_none();
        let startup =
            kirq::try_register_startup(virq, handler.clone(), kirq::IrqRegisterOptions::disabled());
        (ordinary, was_absent, startup, !karch::local_irq_enabled())
    };
    let removed = kirq::try_free_irq(virq);
    let did_remove = matches!(&removed, Ok(Some(_)));
    drop(removed);
    assert!(matches!(
        ordinary,
        Err(kirq::IrqDescError::InvalidContext { .. })
    ));
    assert!(was_absent);
    assert_eq!(startup, Ok(true));
    assert!(stayed_masked);
    assert!(did_remove);
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn read_scope_rejects_normal_registration_before_publication() {
    let virq = 0x5f73;
    assert!(kirq::descriptor(virq).is_none());
    let handler: Arc<dyn IrqHandler> = Arc::new(|_| IrqEvent::HANDLED);
    let (rejected, was_absent) = krcu::with_read(|_| {
        let result = kirq::try_register_disabled(virq, handler.clone());
        (result, kirq::descriptor(virq).is_none())
    });
    // This is an ordinary-admission check only. It deliberately makes no claim
    // about startup registration being valid while retaining an RCU read scope.
    let after_scope = kirq::try_register_disabled(virq, handler.clone());
    let removed = kirq::try_free_irq(virq);
    let did_remove = matches!(&removed, Ok(Some(_)));
    drop(removed);
    assert!(matches!(
        rejected,
        Err(kirq::IrqDescError::InvalidContext { .. })
    ));
    assert!(was_absent);
    assert_eq!(after_scope, Ok(true));
    assert!(did_remove);
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn request_waits_for_free_to_drain_retained_inflight() {
    // This synthetic number is never enabled or passed to a controller. Reject
    // a collision rather than modifying a pre-existing registration.
    let virq = 0x5f70;
    let other_virq = 0x5f71;
    assert!(kirq::descriptor(virq).is_none());
    assert!(kirq::descriptor(other_virq).is_none());
    let old_drops = Arc::new(AtomicUsize::new(0));
    let new_drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        kirq::try_register_disabled(
            virq,
            Arc::new(HandlerProbe {
                drops: old_drops.clone(),
                calls: calls.clone(),
            })
        )
        .unwrap()
    );
    let in_flight = RetainedInFlight::begin(virq).unwrap();
    let free_started = Arc::new(Completion::new());
    let free_succeeded = Arc::new(AtomicBool::new(false));
    let free_task = {
        let started = free_started.clone();
        let succeeded = free_succeeded.clone();
        ktask::spawn(move || {
            started.complete_all();
            let removed = kirq::try_free_irq(virq);
            let did_remove = matches!(&removed, Ok(Some(_)));
            drop(removed);
            succeeded.store(did_remove, Ordering::Release);
        })
    };
    kiface_task_wait_api::wait_for_completion(&free_started).unwrap();
    while !in_flight.is_teardown_in_progress() && free_task.state() != TaskState::Exited {
        ktask::yield_now();
    }
    while !matches!(free_task.state(), TaskState::Blocked | TaskState::Exited) {
        ktask::yield_now();
    }
    let has_teardown_waiter =
        in_flight.is_teardown_in_progress() && free_task.state() == TaskState::Blocked;

    let request_started = Arc::new(Completion::new());
    let request_succeeded = Arc::new(AtomicBool::new(false));
    let request_task = {
        let started = request_started.clone();
        let succeeded = request_succeeded.clone();
        let handler = Arc::new(HandlerProbe {
            drops: new_drops.clone(),
            calls: calls.clone(),
        });
        ktask::spawn(move || {
            started.complete_all();
            succeeded.store(
                kirq::try_register_disabled(virq, handler) == Ok(true),
                Ordering::Release,
            );
        })
    };
    kiface_task_wait_api::wait_for_completion(&request_started).unwrap();
    while !matches!(request_task.state(), TaskState::Blocked | TaskState::Exited) {
        ktask::yield_now();
    }
    // The start completions exclude task-startup blocking. Neither worker has
    // another blocking operation before the IRQ API. State observations confirm
    // actual waits, not merely an intent flag published before lock acquisition.
    let has_request_waiter = request_task.state() == TaskState::Blocked;
    let has_live_descriptor = kirq::descriptor(virq).is_some();
    let old_drops_before_release = old_drops.load(Ordering::Acquire);
    let new_drops_before_release = new_drops.load(Ordering::Acquire);
    let did_free_early = free_succeeded.load(Ordering::Acquire);
    let did_register_early = request_succeeded.load(Ordering::Acquire);

    // A sleeping transaction excludes its own Line, not unrelated management.
    let other_registered = kirq::try_register_disabled(other_virq, Arc::new(|_| IrqEvent::HANDLED));
    let other_removed = kirq::try_free_irq(other_virq);
    let did_remove_other = matches!(&other_removed, Ok(Some(_)));
    drop(other_removed);

    drop(in_flight);
    let free_status = free_task.join();
    let request_status = request_task.join();
    let final_removed = kirq::try_free_irq(virq);
    let did_remove_replacement = matches!(&final_removed, Ok(Some(_)));
    drop(final_removed);

    assert_eq!([free_status, request_status], [0, 0]);
    assert!(has_teardown_waiter);
    assert!(has_request_waiter);
    assert!(has_live_descriptor);
    assert!(!did_free_early);
    assert!(!did_register_early);
    assert_eq!(other_registered, Ok(true));
    assert!(did_remove_other);
    assert!(kirq::descriptor(other_virq).is_none());
    assert_eq!([old_drops_before_release, new_drops_before_release], [0, 0]);
    assert!(free_succeeded.load(Ordering::Acquire));
    assert!(request_succeeded.load(Ordering::Acquire));
    assert!(did_remove_replacement);
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(old_drops.load(Ordering::Acquire), 1);
    assert_eq!(new_drops.load(Ordering::Acquire), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}
