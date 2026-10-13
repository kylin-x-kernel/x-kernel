// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn repeated_enrollment_keeps_original_owner_until_outer_exit() {
    let first = ReaderNode::new();
    let second = ReaderNode::new();
    let reader = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        reader.with_read(|_| {
            // SAFETY: this isolated scheduler fixture is exclusively driven
            // here with IRQs/preemption excluded (no-op on host). No actual
            // task references it; stack-stable storage outlives every removal.
            unsafe { first.enroll(&reader) };
            reader.with_read(|_| {
                // SAFETY: serial duplicate call, with the same exclusion and
                // stable fixture lifetime; membership stays on the first node.
                unsafe { second.enroll(&reader) };
                // SAFETY: same exclusive fixture and lifetime as above.
                unsafe { first.enroll(&reader) };
            });
            (
                reader.is_enrolled_on(&first),
                first.reader_count(),
                second.reader_count(),
            )
        })
    };
    assert_eq!(observed, (true, 1, 0));
    assert!(!reader.is_enrolled());
    assert_eq!(first.reader_count(), 0);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn removal_preserves_other_readers_and_inactive_tasks_do_not_enroll() {
    let node = ReaderNode::new();
    let first = TaskReader::new();
    let second = TaskReader::new();
    let (inactive_count, both_count, after_inner) = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated scheduler fixture, no task or other CPU accesses it;
        // IRQs/preemption excluded, all storage stable through scope removal.
        unsafe { node.enroll(&first) };
        let inactive_count = node.reader_count();
        let (both_count, after_inner) = first.with_read(|_| {
            // SAFETY: same exclusive fixture; first's scope ends before storage.
            unsafe { node.enroll(&first) };
            let both_count = second.with_read(|_| {
                // SAFETY: second has distinct links and the same stable lifetime.
                unsafe { node.enroll(&second) };
                node.reader_count()
            });
            (
                both_count,
                (
                    first.is_enrolled_on(&node),
                    second.is_enrolled(),
                    node.reader_count(),
                ),
            )
        });
        (inactive_count, both_count, after_inner)
    };
    assert_eq!(inactive_count, 0);
    assert_eq!(both_count, 2);
    assert_eq!(after_inner, (true, false, 1));
    assert_eq!(node.reader_count(), 0);
}
