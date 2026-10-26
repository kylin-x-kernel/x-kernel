// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host callback pairing only; this test does not execute user instructions.

use unittest::{assert, assert_eq, def_test};

use crate::host::{with_current_reader, with_preempt_check_deferred};

#[def_test]
fn user_boundary_pair_preserves_cpu_and_interrupt_state() -> unittest::TestResult {
    let _guard = kspin::IrqSave::new();
    with_current_reader(check_user_boundary)
}

fn check_user_boundary(current: crate::host::CurrentReader<'_>) -> unittest::TestResult {
    let reader = current.reader();
    assert!(!kiface_task_api::TaskPreemptionIf::current_preemption_disabled());
    let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    let sequence = watching.sequence_for_test();
    with_preempt_check_deferred(|| {
        let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
            // SAFETY: retained current fixture under IRQ/preemption exclusion;
            // the user-entry boundary below cleans membership on this static node.
            unsafe { super::reader_node(cpu).enroll(reader) };
        };
        // SAFETY: scope-free callback model; task/node remain live through cleanup.
        unsafe { reader.with_read_deferred(enroll_fn) };
    });
    // Model the final architecture callback with no preexisting preempt guard;
    // leave the pending membership for user entry rather than guard-drop cleanup.
    let was_enrolled = reader.is_enrolled();
    // SAFETY: Model the architecture callbacks without entering userspace.
    // The IRQ mask and nonscheduling callbacks exclude migration; no reader is
    // active. The window only samples permanent watching atomics before exit.
    unsafe { super::enter_user_quiescent() };
    let was_quiescent = !watching.is_watching();
    // SAFETY: This matches the entry above on the same IRQ-masked CPU; no
    // interrupt scope, protected access or scheduling occurred in between.
    unsafe { super::exit_user_quiescent() };
    assert!(was_enrolled);
    assert!(!reader.is_enrolled());
    assert!(!kiface_task_api::TaskPreemptionIf::current_preemption_disabled());
    assert!(was_quiescent);
    assert!(watching.is_watching());
    assert!(watching.sequence_for_test().wrapping_sub(sequence) >= 2);
    assert_eq!(kiface_cpu_api::CurrentCpuIf::current_cpu(), cpu);
    assert!(!karch::local_irq_enabled());
    assert!(with_current_reader(|after| core::ptr::eq(after.reader(), reader)));
    unittest::TestResult::Ok
}
