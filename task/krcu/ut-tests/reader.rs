// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn nested_scopes_balance_on_error_return() {
    let reader = TaskReader::new();
    let (outer, inner, after_inner, result) = reader.with_read(|_| {
        let outer = reader.nesting.load(Ordering::Acquire);
        let inner = reader.with_read(|_| reader.nesting.load(Ordering::Acquire));
        let after_inner = reader.nesting.load(Ordering::Acquire);
        (outer, inner, after_inner, Err::<(), _>("scope result"))
    });
    assert_eq!((outer, inner, after_inner), (1, 2, 1));
    assert_eq!(result, Err("scope result"));
    assert!(!reader.is_reading());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn nesting_never_wraps() {
    let reader = TaskReader::new();
    assert_eq!(reader.try_leave(), None);
    assert_eq!(reader.nesting.load(Ordering::Acquire), 0);
    reader.nesting.store(usize::MAX - 1, Ordering::Relaxed);
    assert!(reader.try_enter());
    assert!(!reader.try_enter());
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX);
    assert_eq!(reader.try_leave(), Some(usize::MAX - 1));
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX - 1);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn terminal_cleanup_balances_unenrolled_nesting() {
    let reader = TaskReader::new();
    let (inactive, active, repeated) = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated stable fixture with no executing task or guard;
        // IRQ/preemption exclusion covers all terminal cleanup calls here.
        let inactive = unsafe { reader.finish_task_exit() };
        let entered = reader.try_enter() && reader.try_enter();
        // SAFETY: synthetic nesting has no guard or continuing read accesses.
        let active = unsafe { reader.finish_task_exit() };
        // SAFETY: the same exclusive fixture remains inactive and stable.
        let repeated = unsafe { reader.finish_task_exit() };
        (inactive, entered && active, repeated)
    };
    assert!(!inactive);
    assert!(active);
    assert!(!repeated);
    assert!(!reader.is_reading());
    assert!(!reader.is_enrolled());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn terminal_cleanup_unlinks_original_node_without_removing_other_readers() {
    let original = ReaderNode::new();
    let destination = ReaderNode::new();
    let exited = TaskReader::new();
    let survivor = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // Synthetic nesting models an abandoned stack without leaking guards.
        let entered = exited.try_enter() && exited.try_enter();
        // SAFETY: exclusive fixture, stable storage, IRQ/preemption excluded;
        // terminal cleanup below removes enrollment before storage is dropped.
        unsafe { original.enroll(&exited) };
        // SAFETY: serialized migration attempt on the same retained fixture.
        unsafe { destination.enroll(&exited) };
        survivor.with_read(|_| {
            // SAFETY: distinct stable reader, exclusive fixture; its ordinary
            // outer exit removes membership before the node is destroyed.
            unsafe { original.enroll(&survivor) };
            // SAFETY: no actual task or guard can resume the exited fixture;
            // both nodes and the reader are retained and updates are exclusive.
            let cleaned = unsafe { exited.finish_task_exit() };
            (
                entered && cleaned,
                original.reader_count(),
                destination.reader_count(),
                survivor.is_enrolled_on(&original),
            )
        })
    };
    assert_eq!(observed, (true, 1, 0, true));
    assert!(!exited.is_reading());
    assert!(!exited.is_enrolled());
    assert_eq!(original.reader_count(), 0);
}
