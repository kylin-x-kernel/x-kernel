// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn plain_resource_retains_virq_identity() {
    let resource = IrqResource::plain(Virq::new(32), DevIrqTrigger::Unknown(0));
    assert_eq!(
        irq_resource_to_kirq_spec(resource),
        kirq::IrqSpec::PlainVirq(32)
    );
}

#[def_test]
fn mapped_resource_preserves_explicit_domain_and_distinct_hwirq() {
    let domain = kirq::IrqDomainId::new(87);
    let resource = IrqResource::mapped(
        Virq::new(48),
        DevIrqTrigger::LevelHigh,
        DevIrqController::Gic,
        Some(domain),
        30,
    );
    let expected = kirq::IrqDesc::new(30, DevIrqTrigger::LevelHigh)
        .with_controller(DevIrqController::Gic)
        .with_domain(domain)
        .with_virq(48);
    assert_eq!(
        irq_resource_to_kirq_spec(resource),
        kirq::IrqSpec::Desc(expected)
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
        kirq::IrqDescError::RetirementUnavailable,
        kirq::IrqDescError::InvalidContext {
            operation: "provider test",
        },
    ] {
        assert_eq!(map_irq_desc_error(error), ResError::InvalidResource);
    }
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
    assert_eq!(
        require_released_action(Err(kirq::IrqDescError::RetirementUnavailable)),
        Err(ResError::InvalidResource)
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
