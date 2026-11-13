// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Current-task slot ownership and guard reentry regressions.

use crate::KtaskRef;

use unittest::{assert, assert_eq, def_test};

struct EntryDropProbe(alloc::sync::Arc<core::sync::atomic::AtomicUsize>);

impl Drop for EntryDropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

#[def_test]
fn prepared_kernel_entry_transfers_without_reboxing() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    for (should_run, has_pid) in [(false, false), (true, false), (false, true), (true, true)] {
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = EntryDropProbe(drops.clone());
        let entry = super::PreparedTaskEntry::new(move || drop(probe)).unwrap();
        let address = (&*entry.0) as *const dyn super::OwnedTaskEntry as *const ();
        let task = if has_pid {
            super::TaskInner::new_kthread_with_entry(
                entry, "prepared-kernel-entry".into(), 16384,
            ).unwrap()
        } else {
            super::TaskInner::new_pidless_kthread_with_entry(
                entry, "prepared-kernel-entry".into(), 16384,
            )
        };
        assert_eq!(task.task_number().is_some(), has_pid);
        assert_eq!(matches!(task.identity, super::TaskIdentity::Internal), !has_pid);
        let stored = task.entry.take().unwrap();
        let super::TaskEntry::Prepared(entry) = stored else {
            panic!("expected prepared kernel entry");
        };
        assert_eq!((&*entry.0) as *const dyn super::OwnedTaskEntry as *const (), address);
        drop(task);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        if should_run {
            entry.run();
        } else {
            drop(entry);
        }
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn prepared_consuming_entry_runs_or_cancels_captures_once() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    for should_run in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let probe = EntryDropProbe(drops.clone());
        let entry = super::PreparedTaskEntry::new(move || {
            invoked.fetch_add(1, Ordering::Relaxed);
            drop(probe);
        }).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        if should_run {
            entry.run();
        } else {
            drop(entry);
        }
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(should_run));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn prepared_entry_allocation_failure_drops_without_invoking() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    for flags in [super::AllocFlags::KERNEL, super::AllocFlags::NOWAIT] {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let probe = EntryDropProbe(drops.clone());
        let requested = core::cell::Cell::new(None);
        let result = super::PreparedTaskEntry::new_with_factory(move || {
            invoked.fetch_add(1, Ordering::Relaxed);
            drop(probe);
        }, flags, |_, flags| {
            requested.set(Some(flags));
            Err(kerrno::KError::NoMemory)
        });
        assert_eq!(result.err(), Some(kerrno::KError::NoMemory));
        assert_eq!(requested.get(), Some(flags));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn prepared_zero_sized_entry_does_not_add_adapter_storage() {
    let entry = super::PreparedTaskEntry::new_nowait(|| {}).unwrap();
    assert_eq!(core::alloc::Layout::for_value(&*entry.0).size(), 0);
    entry.run();
}

#[def_test]
fn idle_entry_keeps_a_function_pointer_without_closure_storage() {
    static CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    fn entry() {
        CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    let before = CALLS.load(core::sync::atomic::Ordering::Relaxed);
    let task = super::TaskInner::new_idle(entry, "entry-test".into(), 16384);
    let prepared = task.entry.take().unwrap();
    assert!(matches!(&prepared, super::TaskEntry::Function(_)));
    assert!(task.entry.take().is_none());
    prepared.run();
    assert_eq!(CALLS.load(core::sync::atomic::Ordering::Relaxed), before + 1);
}

#[cfg(feature = "smp")]
#[def_test]
fn task_argument_entry_transfers_and_releases_its_owner_once() {
    let target = super::TaskInner::new_common(
        super::TaskIdentity::Internal,
        "argument-target".into(),
    ).into_arc();
    let before = KtaskRef::strong_count(&target);
    let helper = super::TaskInner::new_internal_with_task(
        |task| task.set_name("invoked"),
        target.clone(),
        "argument-helper".into(),
        16384,
    );
    assert_eq!(KtaskRef::strong_count(&target), before + 1);
    let prepared = helper.entry.take().unwrap();
    assert!(matches!(&prepared, super::TaskEntry::TaskArgument(_, retained) if KtaskRef::ptr_eq(retained, &target)));
    assert!(helper.entry.take().is_none());
    drop(helper);
    assert_eq!(KtaskRef::strong_count(&target), before + 1);
    prepared.run();
    assert_eq!(KtaskRef::strong_count(&target), before);
    assert_eq!(target.name(), "invoked");
}

#[cfg(feature = "smp")]
#[def_test]
fn dropping_unpublished_task_argument_entry_releases_without_invoking() {
    static CALLS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let before_calls = CALLS.load(core::sync::atomic::Ordering::Relaxed);
    let target = super::TaskInner::new_common(
        super::TaskIdentity::Internal,
        "cancel-target".into(),
    ).into_arc();
    let before = KtaskRef::strong_count(&target);
    let helper = super::TaskInner::new_internal_with_task(
        |_| {
            CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        },
        target.clone(),
        "cancel-helper".into(),
        16384,
    );
    assert_eq!(KtaskRef::strong_count(&target), before + 1);
    drop(helper);
    assert_eq!(KtaskRef::strong_count(&target), before);
    assert_eq!(CALLS.load(core::sync::atomic::Ordering::Relaxed), before_calls);
}

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
