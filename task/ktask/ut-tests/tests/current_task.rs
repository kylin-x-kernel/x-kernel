// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Current-task slot ownership and guard reentry regressions.

use crate::KtaskRef;

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn current_task_read_preserves_irq_and_preempt_state() {
    let task = crate::current();
    let irq_enabled = karch::local_irq_enabled();
    #[cfg(feature = "preempt")]
    let can_preempt = task.can_preempt(0);
    assert_eq!(khal::context::current_task_ptr().cast(), KtaskRef::as_ptr(&task));
    assert_eq!(karch::local_irq_enabled(), irq_enabled);
    {
        let _irq = kspin::IrqSave::new();
        let _nested_irq = kspin::IrqSave::new();
        assert_eq!(khal::context::current_task_ptr().cast(), KtaskRef::as_ptr(&task));
        assert!(!karch::local_irq_enabled());
        #[cfg(feature = "preempt")]
        assert_eq!(task.can_preempt(0), can_preempt);
    }
    assert_eq!(karch::local_irq_enabled(), irq_enabled);
    #[cfg(feature = "preempt")]
    assert_eq!(task.can_preempt(0), can_preempt);
}

#[def_test]
fn current_task_publication_retains_old_slot_owner() {
    let owner = crate::current().clone();
    let _irq = kspin::IrqSave::new();
    let count = KtaskRef::strong_count(&owner);
    // Republish the real task, not a fake current task that an NMI could use.
    // SAFETY: normal IRQs/migration are excluded; prev consumes the current
    // slot owner exactly once and next supplies its replacement. owner keeps
    // this same, still-running task and its stack alive throughout the test.
    let (previous, next_ptr) = unsafe { crate::current().prepare_switch(owner.clone()) };
    // Preparation and publication are separate operations, so both sides of
    // the window can be observed without callbacks in the production path.
    // Record before restoring ownership so a failed assertion cannot leave
    // the current pointer without its owning reference.
    let before_count = KtaskRef::strong_count(&crate::current());
    // SAFETY: IRQs/migration remain excluded and next_ptr owns a reference to
    // the real current task. previous retains the old owner through this store.
    unsafe { khal::context::set_current_task_ptr(next_ptr) };
    assert_eq!(before_count, count + 1);
    assert_eq!(KtaskRef::strong_count(&crate::current()), count + 1);
    assert_eq!(
        khal::context::current_task_ptr().cast(),
        KtaskRef::as_ptr(&owner)
    );
    assert!(KtaskRef::ptr_eq(&previous, &owner));
    assert_eq!(KtaskRef::strong_count(&owner), count + 1);
    // A same-task republication does not leave its stack; finish the handoff
    // explicitly after verifying that publication did not release the owner.
    drop(previous);
    assert_eq!(KtaskRef::strong_count(&owner), count);
}

#[def_test]
fn current_task_identity_survives_switches() {
    let worker = crate::spawn(|| {
        let owner = crate::current().clone();
        for _ in 0..128 {
            crate::yield_now();
            let is_current = {
                let _irq = kspin::IrqSave::new();
                khal::context::current_task_ptr().cast() == KtaskRef::as_ptr(&owner)
                    && crate::current().ptr_eq(&owner)
            };
            if !is_current {
                crate::exit(1);
            }
        }
    });
    assert_eq!(worker.join(), 0);
}
