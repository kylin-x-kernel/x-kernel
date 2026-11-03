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
        assert_eq!(
            claim.as_ref().map(kirq::PendingIrq::source),
            Some(kirq::IrqRef::Virq(vector))
        );
        assert_eq!(errors.get(), 0);
    }
}

#[def_test]
fn io_apic_claims_transfer_completion_to_the_selected_generic_flow() {
    for (trigger, expected_early, expected_cookie) in [
        (Some(x86_apic::IoApicTriggerMode::Edge), 0, 1),
        (Some(x86_apic::IoApicTriggerMode::Level), 0, 1),
        // Even an unregistered/out-of-range claim must transfer its EOI;
        // classification itself is not a valid-pin proof.
        (None, 0, 1),
    ] {
        for hwirq in [
            0,
            usize::from(x86_apic::MSIX_VECTOR_BASE) - x86_apic::IO_APIC_VECTOR_BASE - 1,
        ] {
            let queries = Cell::new(0);
            let observed_hwirq = Cell::new(None);
            let eois = Cell::new(0);
            let delivery = super::prepare_regular_delivery(
                x86_apic::IO_APIC_VECTOR_BASE + hwirq,
                |observed| {
                    observed_hwirq.set(Some(observed));
                    queries.set(queries.get() + 1);
                    trigger
                },
                || eois.set(eois.get() + 1),
            );
            assert_eq!(queries.get(), 1);
            assert_eq!(observed_hwirq.get(), Some(hwirq));
            assert_eq!(eois.get(), expected_early);
            assert_eq!(
                delivery,
                Some((
                    kirq::IrqRef::Domain(super::IO_APIC_DOMAIN, hwirq),
                    expected_cookie
                ))
            );
            let (_, cookie) = delivery.expect("classified IO-APIC vector");
            super::complete_delivery(cookie, || eois.set(eois.get() + 1));
            assert_eq!(eois.get(), 1);
        }
    }
}

#[def_test]
fn msi_transfers_ack_while_local_vectors_keep_early_eoi() {
    for (vector, source) in [
        (
            usize::from(x86_apic::MSIX_VECTOR_BASE),
            kirq::IrqRef::Domain(kirq::MSI_DOMAIN, usize::from(x86_apic::MSIX_VECTOR_BASE)),
        ),
        (
            usize::from(x86_apic::APIC_TIMER_VECTOR) - 1,
            kirq::IrqRef::Domain(
                kirq::MSI_DOMAIN,
                usize::from(x86_apic::APIC_TIMER_VECTOR) - 1,
            ),
        ),
        (
            usize::from(x86_apic::APIC_TIMER_VECTOR),
            kirq::IrqRef::Virq(usize::from(x86_apic::APIC_TIMER_VECTOR)),
        ),
        (0xf3, kirq::IrqRef::Virq(0xf3)),
        (0xff, kirq::IrqRef::Virq(0xff)),
    ] {
        let eois = Cell::new(0);
        let delivery = super::prepare_regular_delivery(
            vector,
            |_| panic!("MSI/local vector must not access IO-APIC registers"),
            || eois.set(eois.get() + 1),
        );
        let is_local = vector >= usize::from(x86_apic::APIC_TIMER_VECTOR);
        assert_eq!(delivery, Some((source, usize::from(!is_local))));
        assert_eq!(eois.get(), usize::from(is_local));
        let (_, cookie) = delivery.expect("classified MSI/local vector");
        super::complete_delivery(cookie, || eois.set(eois.get() + 1));
        assert_eq!(eois.get(), 1);
    }
}

#[def_test]
fn source_flow_selection_separates_level_edge_msi_and_local() {
    use kirq::{IrqFlow, IrqTrigger};
    for (trigger, hardware, is_edge) in [
        (
            IrqTrigger::EdgeRising,
            x86_apic::IoApicTriggerMode::Level,
            true,
        ),
        (
            IrqTrigger::LevelLow,
            x86_apic::IoApicTriggerMode::Edge,
            false,
        ),
        (
            IrqTrigger::Unknown(0),
            x86_apic::IoApicTriggerMode::Edge,
            true,
        ),
        (
            IrqTrigger::Unknown(0),
            x86_apic::IoApicTriggerMode::Level,
            false,
        ),
    ] {
        let mut desc = super::legacy_irq_desc(4);
        desc.trigger = trigger;
        let flow = super::select_x86_flow(desc, |_| Some(hardware)).unwrap();
        assert_eq!(matches!(flow, IrqFlow::Edge(_)), is_edge);
        assert_eq!(matches!(flow, IrqFlow::Fasteoi(_)), !is_edge);
    }
    let msi = kirq::IrqDesc::new(0x40, IrqTrigger::EdgeRising)
        .with_domain(kirq::MSI_DOMAIN)
        .with_controller(kirq::IrqController::Msi)
        .with_flags(kirq::IrqFlags::MSI);
    assert!(matches!(
        super::select_x86_flow(msi, |_| panic!("MSI is not IO-APIC")),
        Ok(IrqFlow::Edge(_))
    ));
    assert_eq!(
        super::select_x86_flow(kirq::IrqDesc::from_virq(0xf0), |_| panic!("local vector")),
        Ok(IrqFlow::Legacy)
    );
}

#[def_test]
fn unsupported_source_metadata_is_rejected_before_register_access() {
    let io = super::legacy_irq_desc(4);
    for desc in [
        super::legacy_irq_desc(32),
        io.with_flags(kirq::IrqFlags::PER_CPU),
        io.with_domain(kirq::MSI_DOMAIN),
        io.with_affinity(kirq::IrqAffinity::Cpu(1)),
    ] {
        assert!(
            super::select_x86_flow(desc, |_| panic!("invalid metadata must not read registers"))
                .is_err()
        );
    }
    assert!(super::select_x86_flow(io, |_| None).is_err());
}

#[def_test]
fn exception_vectors_do_not_claim_or_complete_an_irq() {
    for vector in [0, x86_apic::IO_APIC_VECTOR_BASE - 1] {
        assert!(
            super::prepare_regular_delivery(
                vector,
                |_| panic!("exception vector must not access IO-APIC registers"),
                || panic!("exception vector must not issue LAPIC EOI"),
            )
            .is_none()
        );
    }
}
