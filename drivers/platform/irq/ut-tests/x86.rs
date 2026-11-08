// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::Cell;

use kirq::{IrqPolarity, IrqTrigger};
use unittest::{assert, assert_eq, def_test};

fn io_desc(pin: usize) -> kirq::IrqDesc {
    kirq::IrqDesc::new(pin, IrqTrigger::EdgeRising)
        .with_source(kirq::IrqSource::PlatformStatic)
        .with_controller(kirq::IrqController::IoApic)
        .with_domain(super::IO_APIC_DOMAIN)
}

#[def_test(serial)]
fn spurious_vector_returns_through_common_irq_exit() {
    // Check common entry/exit state using the dedicated no-EOI vector. This
    // software invocation does not simulate a hardware spurious delivery;
    // fixed_vectors_bypass_regular_claim_and_completion checks classification.
    let _guard = kspin::NoPreemptIrqSave::new();
    assert!(!kirq::context::is_in_interrupt_context());
    // SAFETY: the guard supplies CPU/IRQ exclusion on this initialized CPU;
    // the spurious vector cannot claim or complete a hardware interrupt.
    unsafe { kirq::handle_irq(usize::from(x86_apic::APIC_SPURIOUS_VECTOR)) };
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
                None::<()>
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
                Some(kirq::IrqRef::Virq(received))
            },
            || errors.set(errors.get() + 1),
        );
        assert_eq!(observed_vector.get(), Some(vector));
        assert_eq!(
            claim,
            Some(kirq::IrqRef::Virq(vector))
        );
        assert_eq!(errors.get(), 0);
    }
}

#[def_test]
fn io_apic_claims_transfer_completion_to_the_selected_generic_flow() {
    for hwirq in [
        0,
        usize::from(x86_apic::MSIX_VECTOR_BASE) - x86_apic::IO_APIC_VECTOR_BASE - 1,
    ] {
        let queries = Cell::new(0);
        let observed_vector = Cell::new(None);
        let eois = Cell::new(0);
        let vector = x86_apic::IO_APIC_VECTOR_BASE + hwirq;
        let delivery = super::prepare_regular_delivery(
            vector,
            |observed| {
                observed_vector.set(Some(observed));
                queries.set(queries.get() + 1);
                Some((super::IO_APIC_DOMAIN, hwirq))
            },
            || eois.set(eois.get() + 1),
        );
        assert_eq!(queries.get(), 1);
        assert_eq!(observed_vector.get(), Some(vector));
        assert_eq!(eois.get(), 0);
        assert_eq!(
            delivery,
            Some((kirq::IrqRef::Domain(super::IO_APIC_DOMAIN, hwirq), 1))
        );
        let (_, cookie) = delivery.expect("classified IO-APIC vector");
        super::complete_delivery(cookie, || eois.set(eois.get() + 1));
        assert_eq!(eois.get(), 1);
    }
}

#[def_test]
fn unclassified_io_apic_vector_is_acknowledged_and_dropped() {
    // A vector already in service whose route disappeared must still be
    // acknowledged; otherwise the local APIC keeps it pending forever.
    let eois = Cell::new(0);
    assert_eq!(
        super::prepare_regular_delivery(
            x86_apic::IO_APIC_VECTOR_BASE,
            |_| None,
            || eois.set(eois.get() + 1),
        ),
        None
    );
    assert_eq!(eois.get(), 1);
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
        let mut desc = io_desc(4);
        desc.trigger = trigger;
        let flow = super::select_x86_flow(desc, |_, _| Some(hardware)).unwrap();
        assert_eq!(matches!(flow, IrqFlow::Edge(_)), is_edge);
        assert_eq!(matches!(flow, IrqFlow::Fasteoi(_)), !is_edge);
    }
    let msi = kirq::IrqDesc::new(0x40, IrqTrigger::EdgeRising)
        .with_domain(kirq::MSI_DOMAIN)
        .with_controller(kirq::IrqController::Msi)
        .with_flags(kirq::IrqFlags::MSI);
    assert!(matches!(
        super::select_x86_flow(msi, |_, _| panic!("MSI is not IO-APIC")),
        Ok(IrqFlow::Edge(_))
    ));
    assert_eq!(
        super::select_x86_flow(kirq::IrqDesc::from_virq(0xf0), |_, _| panic!("local vector")),
        Ok(IrqFlow::PerCpu)
    );
}

#[def_test]
fn unsupported_source_metadata_is_rejected_before_register_access() {
    let io = io_desc(4);
    for desc in [
        io_desc(32),
        io.with_flags(kirq::IrqFlags::PER_CPU),
        io.with_domain(kirq::MSI_DOMAIN),
        io.with_affinity(kirq::IrqAffinity::Cpu(1)),
    ] {
        assert!(
            super::select_x86_flow(desc, |_, _| panic!("invalid metadata must not read registers"))
                .is_err()
        );
    }
    assert!(super::select_x86_flow(io, |_, _| None).is_err());
}

#[def_test]
fn unknown_and_foreign_sources_are_rejected_without_register_access() {
    use kirq::{IrqDesc, IrqDescError};
    for desc in [
        IrqDesc::from_hwirq(4),
        IrqDesc::from_virq(kirq::DYNAMIC_VIRQ_BASE),
        kirq::gic_level_irq_desc(32),
        kirq::plic_irq_desc(4),
    ] {
        assert_eq!(
            super::select_x86_flow(desc, |_, _| panic!("unknown source must not read registers")),
            Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq })
        );
    }
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

fn declaration(address: u32, gsi_base: u32) -> super::IoApicDeclaration {
    super::IoApicDeclaration {
        id: 0,
        address,
        gsi_base,
    }
}

#[def_test]
fn static_configuration_is_accepted_before_hardware_access() {
    let config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert_eq!(config.source, fwnode::FwSource::Static);
    assert_eq!(super::validate_config(&config), Ok(()));
}

#[def_test]
fn firmware_configuration_keeps_acpi_provenance() {
    let config = super::X86IrqInitConfig::from_acpi(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert_eq!(config.source, fwnode::FwSource::Acpi);
    assert_eq!(super::validate_config(&config), Ok(()));
}

#[def_test]
fn zero_address_is_invalid_input() {
    let config = super::X86IrqInitConfig::platform_static(
        declaration(0, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert_eq!(
        super::validate_config(&config),
        Err(kerrno::KError::InvalidInput)
    );
}

#[def_test]
fn non_zero_gsi_base_is_accepted_before_hardware_access() {
    let config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 24),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert_eq!(super::validate_config(&config), Ok(()));
}

#[def_test]
fn unknown_provenance_is_invalid_input() {
    let mut config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    config.source = fwnode::FwSource::Unknown;
    assert_eq!(
        super::validate_config(&config),
        Err(kerrno::KError::InvalidInput)
    );
}

#[def_test]
fn identity_route_builds_a_controller_local_pin_descriptor() {
    let config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    let desc = config.legacy_irq_desc(4).expect("identity ISA 4 route");
    assert_eq!(desc.hwirq, 4);
    assert_eq!(desc.trigger, IrqTrigger::EdgeRising);
    assert_eq!(desc.polarity, IrqPolarity::High);
    assert_eq!(desc.source, kirq::IrqSource::PlatformStatic);
    assert_eq!(desc.controller, kirq::IrqController::IoApic);
    assert_eq!(desc.domain, Some(super::IO_APIC_DOMAIN));
}

#[def_test]
fn override_route_carries_firmware_trigger_polarity_and_pin() {
    let mut config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    config.set_legacy_override(4, 9, IrqTrigger::LevelLow);

    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(desc.hwirq, 9);
    assert_eq!(desc.trigger, IrqTrigger::LevelLow);
    assert_eq!(desc.polarity, IrqPolarity::Low);
}

#[def_test]
fn non_zero_base_translates_gsi_without_aliasing_the_pin() {
    let mut config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 24),
        super::IoApicInitPolicy::platform_legacy(),
    );
    // Identity GSI 4 lies below this controller's base, so there is no route.
    assert!(config.legacy_irq_desc(4).is_none());

    config.set_legacy_override(4, 28, IrqTrigger::LevelLow);
    let desc = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(desc.hwirq, 4, "hwirq is the local pin, not GSI 28");
    assert_eq!(desc.trigger, IrqTrigger::LevelLow);
}

#[def_test]
fn isa_line_outside_the_legacy_range_has_no_descriptor() {
    let config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert!(config.legacy_irq_desc(16).is_none());
}

#[def_test]
fn explicit_route_outside_the_probed_range_fails_validation() {
    let capabilities = |pin_count| x86_apic::IoApicCapabilities { pin_count };

    let mut config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 24),
        super::IoApicInitPolicy::platform_legacy(),
    );
    config.set_legacy_override(4, 60, IrqTrigger::EdgeRising);
    assert_eq!(
        super::validate_legacy_routes(&config, &[capabilities(24)]),
        Err(kerrno::KError::InvalidInput),
        "explicit GSI outside base + pin_count must not fall back to identity"
    );
    assert_eq!(
        super::validate_legacy_routes(&config, &[capabilities(32)]),
        Err(kerrno::KError::InvalidInput),
        "controller-local pin 36 has no fixed vector"
    );

    // Unresolvable identity defaults are skipped rather than failing boot.
    let identity = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 24),
        super::IoApicInitPolicy::platform_legacy(),
    );
    assert_eq!(
        super::validate_legacy_routes(&identity, &[capabilities(24)]),
        Ok(())
    );
}

#[def_test]
fn routed_request_is_stable_for_shared_consumers() {
    let mut config = super::X86IrqInitConfig::platform_static(
        declaration(0xFEC0_0000, 0),
        super::IoApicInitPolicy::platform_legacy(),
    );
    config.set_legacy_override(4, 9, IrqTrigger::LevelLow);

    let first = config.legacy_irq_desc(4).expect("routed ISA 4");
    let second = config.legacy_irq_desc(4).expect("routed ISA 4");
    assert_eq!(first, second);
    let shared = first.with_flags(kirq::IrqFlags::SHARED);
    assert_eq!(shared.domain, first.domain);
    assert_eq!(shared.hwirq, first.hwirq);
}

#[def_test]
fn pin_window_must_fit_below_the_msi_vector_pool() {
    let controller = declaration(0xFEC0_0000, 0);
    let capabilities = |pin_count| x86_apic::IoApicCapabilities { pin_count };
    for rejected in [0, 33, 256] {
        assert_eq!(
            super::validate_capabilities(&controller, capabilities(rejected)),
            Err(kerrno::KError::Unsupported),
            "pin_count={rejected} must not be representable"
        );
    }
    assert_eq!(super::validate_capabilities(&controller, capabilities(32)), Ok(()));
}

#[def_test]
fn gsi_base_plus_pin_count_must_not_overflow() {
    assert_eq!(
        super::validate_capabilities(
            &declaration(0xFEC0_0000, u32::MAX),
            x86_apic::IoApicCapabilities { pin_count: 1 }
        ),
        Err(kerrno::KError::Unsupported)
    );
}

#[def_test]
fn acpi_discovery_reports_the_boot_io_apic() {
    let discovered = super::discovery::from_acpi().expect("QEMU MADT with an IO-APIC");
    assert_eq!(discovered.source(), fwnode::FwSource::Acpi);
    assert!(!discovered.io_apics().is_empty());
    assert!(
        discovered
            .io_apics()
            .iter()
            .all(|controller| controller.address != 0)
    );
}

#[def_test]
fn multiple_controllers_keep_distinct_domains_and_local_pins() {
    let policy = super::IoApicInitPolicy::platform_legacy();
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 24),
    ];
    let mut config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("two controller instances");

    assert_eq!(config.controller_count(), 2);
    let instances: alloc::vec::Vec<_> = config.controllers().collect();
    assert_eq!(instances[0].domain, super::IO_APIC_DOMAIN);
    assert_ne!(instances[0].domain, instances[1].domain);

    // Identity ISA 4 stays on the first controller; a firmware route to GSI 28
    // names the second controller's local pin 4.
    let first = config.legacy_irq_desc(4).expect("identity route on A");
    assert_eq!(first.hwirq, 4);
    assert_eq!(first.domain, Some(instances[0].domain));

    config.set_legacy_override(4, 28, IrqTrigger::LevelLow);
    let second = config.legacy_irq_desc(4).expect("override route on B");
    assert_eq!(second.hwirq, 4, "hwirq is the local pin on B, not GSI 28");
    assert_eq!(second.trigger, IrqTrigger::LevelLow);
    assert_eq!(second.domain, Some(instances[1].domain));

    let capabilities = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 24 },
    ];
    assert_eq!(super::validate_ranges(&config, &capabilities), Ok(()));
    assert_eq!(
        super::validate_legacy_routes(&config, &capabilities),
        Ok(())
    );
}

#[def_test]
fn overlapping_gsi_ranges_are_rejected_before_programming() {
    let policy = super::IoApicInitPolicy::platform_legacy();
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 16),
    ];
    let config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("two controller instances");
    let capabilities = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 24 },
    ];
    assert_eq!(
        super::validate_ranges(&config, &capabilities),
        Err(kerrno::KError::InvalidInput)
    );

    // Adjacent half-open ranges do not overlap.
    let adjacent = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 24),
    ];
    let config =
        super::X86IrqInitConfig::from_acpi_controllers(&adjacent, policy)
            .expect("two controller instances");
    assert_eq!(super::validate_ranges(&config, &capabilities), Ok(()));
}

#[def_test]
fn duplicate_controller_addresses_are_rejected_before_probing() {
    let policy = super::IoApicInitPolicy::platform_legacy();
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_0000, 24),
    ];
    let config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("declarations are individually valid");
    assert_eq!(
        super::validate_config(&config),
        Err(kerrno::KError::InvalidInput)
    );
}

#[def_test]
fn unknown_trigger_still_installs_a_delivery_route() {
    let mut desc = io_desc(4);
    desc.trigger = IrqTrigger::Unknown(0);
    desc.polarity = IrqPolarity::Unknown;
    let observed = Cell::new(None);
    super::configure_with(desc, super::io_pin, |instance, pin, policy| {
        observed.set(Some((instance.as_usize(), pin.as_usize(), policy)));
        true
    })
    .expect("missing trigger metadata must not skip route installation");

    assert_eq!(
        observed.get(),
        Some((
            0,
            4,
            x86_apic::IoApicTriggerPolicy::KeepHardware
        )),
        "the vector is installed while the hardware trigger/polarity is retained"
    );
}

#[def_test]
fn explicit_route_must_agree_with_the_pre_probe_descriptor_owner() {
    let policy = super::IoApicInitPolicy::platform_legacy();
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 16),
    ];
    let mut config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("two controller instances");
    let declared_b = config
        .controllers()
        .nth(1)
        .expect("controller B")
        .domain;
    // Before probing, GSI 20 belongs to B (offset 4). If B reports only two
    // pins, A owns GSI 20 after probing while the pre-probe descriptor already
    // named B; consumers may retain that descriptor, so boot must fail instead.
    config.set_legacy_override(4, 20, IrqTrigger::EdgeRising);
    let short_b = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 2 },
    ];
    assert_eq!(
        super::validate_legacy_routes(&config, &short_b),
        Err(kerrno::KError::InvalidInput)
    );
    assert_eq!(
        config.legacy_irq_desc(4).expect("declared route").domain,
        Some(declared_b),
        "the pre-probe descriptor names B even though B cannot own the GSI later"
    );

    // Adjacent ranges with a probed owner that matches the declared one are
    // accepted. GSI 28 is B/pin 4 both before and after probing.
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 24),
    ];
    let mut config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("two controller instances");
    config.set_legacy_override(4, 28, IrqTrigger::EdgeRising);
    let adjacent = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 20 },
    ];
    assert_eq!(super::validate_legacy_routes(&config, &adjacent), Ok(()));
}

#[def_test]
fn explicit_route_outside_every_probed_instance_range_is_rejected() {
    let policy = super::IoApicInitPolicy::platform_legacy();
    let controllers = [
        declaration(0xFEC0_0000, 0),
        declaration(0xFEC0_1000, 24),
    ];
    let mut config =
        super::X86IrqInitConfig::from_acpi_controllers(&controllers, policy)
            .expect("two controller instances");
    config.set_legacy_override(4, 28, IrqTrigger::EdgeRising);
    assert_eq!(
        config.legacy_irq_desc(4).expect("declared route").hwirq,
        4,
        "the pre-probe descriptor names B/pin 4"
    );

    // B reports only four pins, so its probed range is [24, 28). GSI 28 no
    // longer resolves to any instance; the pre-probe descriptor must not be
    // honored through the identity fallback.
    let short_b = [
        x86_apic::IoApicCapabilities { pin_count: 24 },
        x86_apic::IoApicCapabilities { pin_count: 4 },
    ];
    assert_eq!(
        super::validate_legacy_routes(&config, &short_b),
        Err(kerrno::KError::InvalidInput)
    );
}
