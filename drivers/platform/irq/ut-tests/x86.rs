// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use unittest::{assert, assert_eq, def_test};

#[def_test(serial)]
fn spurious_vector_returns_through_common_irq_exit() {
    // Check common entry/exit state using the dedicated no-EOI vector. This
    // software invocation does not simulate a hardware spurious delivery;
    // fixed_vectors_bypass_regular_claim_and_completion checks classification.
    let _guard = kspin::NoPreemptIrqSave::new();
    assert!(!kirq::context::is_in_interrupt_context());
    kirq::handle_irq(usize::from(x86_apic::APIC_SPURIOUS_VECTOR));
    assert!(!kirq::context::is_in_interrupt_context());
    assert!(!karch::local_irq_enabled());
}

#[def_test]
fn fixed_vectors_bypass_regular_claim_and_completion() {
    for (vector, expected_errors) in [
        (x86_apic::APIC_SPURIOUS_VECTOR, 0),
        (x86_apic::APIC_ERROR_VECTOR, 1),
    ] {
        let regular_calls = Cell::new(0);
        let errors = Cell::new(0);
        let claim = super::dispatch_irq_with(
            usize::from(vector),
            |_| {
                regular_calls.set(regular_calls.get() + 1);
                None
            },
            || errors.set(errors.get() + 1),
        );
        assert!(claim.is_none());
        assert_eq!(regular_calls.get(), 0);
        assert_eq!(errors.get(), expected_errors);
    }
}

#[def_test]
fn non_fixed_vectors_delegate_to_regular_dispatch() {
    // Verify delegation only, not the regular backend's domain classification
    // or hardware EOI behavior. Local timer and IPI vectors share that path.
    for vector in [
        x86_apic::IO_APIC_VECTOR_BASE,
        usize::from(x86_apic::MSIX_VECTOR_BASE),
        usize::from(x86_apic::APIC_TIMER_VECTOR),
        kbuild_config::IPI_IRQ,
    ] {
        let observed_vector = Cell::new(None);
        let errors = Cell::new(0);
        let claim = super::dispatch_irq_with(
            vector,
            |received| {
                observed_vector.set(Some(received));
                // Cookie zero has no hardware completion side effect.
                Some(kirq::PendingIrq::new(kirq::IrqRef::Virq(received), 0))
            },
            || errors.set(errors.get() + 1),
        );
        assert_eq!(observed_vector.get(), Some(vector));
        assert_eq!(claim.as_ref().map(kirq::PendingIrq::source), Some(kirq::IrqRef::Virq(vector)));
        assert_eq!(errors.get(), 0);
    }
}
