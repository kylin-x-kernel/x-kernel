// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host callback pairing only; this test does not execute user instructions.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn user_boundary_pair_preserves_cpu_and_interrupt_state() {
    let _guard = kspin::IrqSave::new();
    let task = ktask::current();
    assert!(!ktask::current_preemption_disabled());
    let cpu = khal::percpu::this_cpu_id();
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    let sequence = watching.sequence_for_test();
    ktask::with_preempt_check_deferred(|| {
        let enroll_fn = |_: &krcu_core::ReadGuard<'_>| {
            // SAFETY: retained current fixture under IRQ/preemption exclusion;
            // the user-entry boundary below cleans membership on this static node.
            unsafe { super::reader_node(cpu).enroll(crate::test_support::reader(&task)) };
        };
        // SAFETY: scope-free callback model; task/node remain live through cleanup.
        unsafe { crate::test_support::reader(&task).with_read_deferred(enroll_fn) };
    });
    // Model the final architecture callback with no preexisting preempt guard;
    // leave the pending membership for user entry rather than guard-drop cleanup.
    let was_enrolled = crate::test_support::reader(&task).is_enrolled();
    // SAFETY: Model the architecture callbacks without entering userspace.
    // The IRQ mask and nonscheduling callbacks exclude migration; no reader is
    // active. The window only samples permanent watching atomics before exit.
    unsafe { super::enter_user_quiescent() };
    let was_quiescent = !watching.is_watching();
    // SAFETY: This matches the entry above on the same IRQ-masked CPU; no
    // interrupt scope, protected access or scheduling occurred in between.
    unsafe { super::exit_user_quiescent() };
    assert!(was_enrolled);
    assert!(!crate::test_support::reader(&task).is_enrolled());
    assert!(!ktask::current_preemption_disabled());
    assert!(was_quiescent);
    assert!(watching.is_watching());
    assert!(watching.sequence_for_test().wrapping_sub(sequence) >= 2);
    assert_eq!(khal::percpu::this_cpu_id(), cpu);
    assert!(!karch::local_irq_enabled());
    assert!(ktask::current().ptr_eq(&task));
}
