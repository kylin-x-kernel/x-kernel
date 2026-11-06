// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[cfg(target_arch = "x86_64")]
#[def_test(serial)]
fn allocated_msi_owner_survives_live_action_rejection() {
    let provider = XKernelResourceProvider;
    let resource = provider.alloc_msix().unwrap();
    let irq = resource.irq;
    let message = resource.message;
    // No device is programmed with this message, so this fixture has no producer.
    let token = provider
        .request_irq(irq, Arc::new(|_| DevIrqEvent::HANDLED))
        .unwrap();
    let (error, retained) = provider.try_free_msix(resource).unwrap_err();
    assert_eq!(error, ResError::Busy);
    assert_eq!(retained, MsiResource::new(irq, message));
    unittest::assert!(kirq::descriptor(irq.virq().raw()).is_some());

    provider.release_irq(irq, token);
    assert_eq!(provider.try_free_msix(retained), Ok(()));
    unittest::assert!(kirq::descriptor(irq.virq().raw()).is_none());
}

#[def_test]
fn msi_rejection_returns_unchanged_resource_for_retry() {
    let irq = IrqResource::plain(Virq::new(48), DevIrqTrigger::EdgeRising);
    let message = driver_os_api::MsiMessage::new(0xfee0_1000, 0x52);
    let (error, resource) = msix_release_result(MsiResource::new(irq, message), false).unwrap_err();
    assert_eq!(error, ResError::Busy);
    assert_eq!(resource, MsiResource::new(irq, message));
    assert_eq!(msix_release_result(resource, true), Ok(()));
}

#[cfg(not(target_arch = "x86_64"))]
#[def_test]
fn non_x86_msi_release_returns_unsupported_with_original_resource() {
    let irq = IrqResource::plain(Virq::new(48), DevIrqTrigger::EdgeRising);
    let message = driver_os_api::MsiMessage::new(0xfee0_1000, 0x52);
    assert_eq!(
        XKernelResourceProvider.try_free_msix(MsiResource::new(irq, message)),
        Err((ResError::Unsupported, MsiResource::new(irq, message)))
    );
}

#[def_test]
fn plain_resource_retains_virq_identity() {
    let resource = IrqResource::plain(Virq::new(32), DevIrqTrigger::Unknown(0));
    assert_eq!(
        irq_resource_to_kirq_spec(resource),
        kirq::IrqSpec::PlainVirq(32)
    );
}

#[def_test]
fn registration_and_line_control_resolve_both_shapes_by_logical_number() {
    let domain = kirq::IrqDomainId::new(87);
    let mapped = IrqResource::mapped(
        Virq::new(48),
        DevIrqTrigger::LevelHigh,
        DevIrqController::Gic,
        Some(domain),
        30,
    );
    let plain = IrqResource::plain(Virq::new(48), DevIrqTrigger::LevelHigh);
    // The IRQ core owns the descriptor created by the mapping. Rebuilding one
    // here from retained route metadata would bypass its authoritative state.
    assert_eq!(
        irq_resource_to_kirq_spec(mapped),
        kirq::IrqSpec::PlainVirq(48)
    );
    assert_eq!(
        irq_resource_to_kirq_spec(plain),
        kirq::IrqSpec::PlainVirq(48)
    );
}

#[def_test]
fn route_domain_overrides_root_and_missing_domain_uses_controller_root() {
    for (controller, root) in [
        (DevIrqController::Gic, kirq::GIC_ROOT_DOMAIN),
        (DevIrqController::Plic, kirq::PLIC_ROOT_DOMAIN),
        (DevIrqController::IoApic, kirq::IO_APIC_DOMAIN),
    ] {
        let mut route = IrqRouteDesc {
            hwirq: 30,
            trigger: DevIrqTrigger::LevelHigh,
            controller,
            domain: None,
        };
        assert_eq!(irq_route_to_kirq_desc(route).domain, Some(root));
        let explicit = kirq::IrqDomainId::new(87);
        route.domain = Some(explicit);
        assert_eq!(irq_route_to_kirq_desc(route).domain, Some(explicit));
    }
}

#[def_test]
fn unknown_controller_does_not_invent_root_domain() {
    for controller in [
        DevIrqController::Unknown,
        DevIrqController::LoongArchExtioi,
        DevIrqController::Msi,
    ] {
        let mut route = IrqRouteDesc {
            hwirq: 30,
            trigger: DevIrqTrigger::EdgeRising,
            controller,
            domain: None,
        };
        assert_eq!(irq_route_to_kirq_desc(route).domain, None);
        route.domain = Some(kirq::IrqDomainId::new(87));
        assert_eq!(irq_route_to_kirq_desc(route).domain, route.domain);
    }
}

#[def_test]
fn ordinary_events_do_not_create_threaded_wakes_or_source_bits() {
    for (input, handled) in [
        (DevIrqEvent::NOT_HANDLED, false),
        (DevIrqEvent::HANDLED, true),
        (DevIrqEvent::WAKE_THREAD, true),
    ] {
        let event = dev_irq_event_to_kirq(input);
        assert_eq!(event.handled(), handled);
        let expected = if handled {
            kirq::IrqEvent::HANDLED
        } else {
            kirq::IrqEvent::NOT_HANDLED
        };
        assert_eq!(event, expected);
        assert_eq!(event.sources(), 0);
    }
}

#[def_test]
fn irq_error_classification_retains_mainline_contract() {
    assert_eq!(
        map_irq_desc_error(kirq::IrqDescError::NoMemory),
        ResError::NoMemory
    );
    assert_eq!(
        map_irq_desc_error(kirq::IrqDescError::VirqExhausted { next: 4096 }),
        ResError::NoMemory
    );
    for error in [
        kirq::IrqDescError::InvalidHwirq { hwirq: usize::MAX },
        kirq::IrqDescError::ConfigurationFailed { hwirq: 30 },
        kirq::IrqDescError::UnknownDomain {
            domain: kirq::IrqDomainId::new(87),
        },
        kirq::IrqDescError::UnknownIrq,
        kirq::IrqDescError::InvalidContext {
            operation: "provider test",
        },
    ] {
        assert_eq!(map_irq_desc_error(error), ResError::InvalidResource);
    }
}

#[def_test]
fn firmware_mapping_errors_keep_readiness_declaration_and_conflict_apart() {
    assert_eq!(
        map_fw_irq_error(kirq::FwIrqMapError::ControllerNotReady),
        ResError::BadState
    );
    for error in [
        kirq::FwIrqMapError::MalformedSpec,
        kirq::FwIrqMapError::InvalidHwirq,
    ] {
        assert_eq!(map_fw_irq_error(error), ResError::InvalidResource);
    }
    assert_eq!(
        map_fw_irq_error(kirq::FwIrqMapError::UnsupportedSpec),
        ResError::Unsupported
    );
    for error in [
        kirq::IrqDescError::HwirqConflict {
            existing: 30,
            newer: 31,
        },
        kirq::IrqDescError::DomainConflict {
            existing: Some(kirq::IrqDomainId::new(1)),
            newer: Some(kirq::IrqDomainId::new(2)),
        },
        kirq::IrqDescError::TriggerConflict {
            existing: DevIrqTrigger::LevelHigh,
            newer: DevIrqTrigger::EdgeRising,
        },
        kirq::IrqDescError::PolarityConflict {
            existing: kirq::IrqPolarity::High,
            newer: kirq::IrqPolarity::Low,
        },
        kirq::IrqDescError::ControllerConflict {
            existing: DevIrqController::Gic,
            newer: DevIrqController::Plic,
        },
        kirq::IrqDescError::MappingConflict {
            domain: kirq::IrqDomainId::new(1),
            hwirq: 30,
            existing: 40,
            newer: 41,
        },
    ] {
        assert_eq!(
            map_fw_irq_error(kirq::FwIrqMapError::Map(error)),
            ResError::AlreadyExists
        );
    }
    assert_eq!(
        map_fw_irq_error(kirq::FwIrqMapError::Map(kirq::IrqDescError::NoMemory)),
        ResError::NoMemory
    );
}

#[def_test]
fn missing_release_action_cannot_authorize_cleanup() {
    assert_eq!(require_released_action(Ok(true)), Ok(()));
    assert_eq!(
        require_released_action(Ok(false)),
        Err(ResError::InvalidResource)
    );
    assert_eq!(
        require_released_action(Err(kirq::IrqDescError::UnknownIrq)),
        Err(ResError::InvalidResource)
    );
    assert_eq!(
        require_released_action(Err(kirq::IrqDescError::NoMemory)),
        Err(ResError::NoMemory)
    );
}

#[def_test]
fn threaded_entries_remain_unsupported_without_registering_an_action() {
    let provider = XKernelResourceProvider;
    let irq = IrqResource::plain(Virq::new(32), DevIrqTrigger::LevelHigh);
    assert_eq!(
        provider.request_threaded_irq(
            irq,
            Arc::new(|_| DevIrqEvent::WAKE_THREAD),
            Arc::new(|_| DevIrqEvent::HANDLED),
            Some("unsupported-thread"),
        ),
        Err(ResError::Unsupported)
    );
    assert_eq!(
        provider.request_threaded_irq_default(
            irq,
            Arc::new(|_| DevIrqEvent::HANDLED),
            Some("unsupported-default-thread"),
        ),
        Err(ResError::Unsupported)
    );
}
