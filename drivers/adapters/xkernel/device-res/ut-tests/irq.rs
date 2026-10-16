// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn test_plain_irq_resource_maps_to_plain_virq_spec() {
    let resource = IrqResource::new(32, DevIrqTrigger::Unknown(0));

    let spec = irq_resource_to_kirq_spec(resource);

    assert_eq!(spec, kirq::IrqSpec::PlainVirq(32));
}

#[def_test]
fn test_routed_irq_resource_maps_to_descriptor_spec() {
    let resource = IrqResource::new(48, DevIrqTrigger::LevelHigh)
        .with_controller(DevIrqController::Gic)
        .with_hwirq(30);

    let spec = irq_resource_to_kirq_spec(resource);
    let kirq::IrqSpec::Desc(desc) = spec else {
        panic!("routed IRQ resource should map to descriptor spec");
    };

    assert_eq!(desc.logical_irq(), Some(48));
    assert_eq!(desc.hwirq, 30);
    assert_eq!(desc.domain, Some(kirq::GIC_ROOT_DOMAIN));
}

#[def_test]
fn test_device_irq_event_wake_request_maps_to_handled_until_threadirq_is_available() {
    let event = dev_irq_event_to_kirq(DevIrqEvent::wake_thread_from_sources(0b1010));

    assert!(event.handled());
    assert_eq!(event.sources(), 0b1010);
}

#[def_test]
fn test_device_irq_event_handled_does_not_wake_thread() {
    let event = dev_irq_event_to_kirq(DevIrqEvent::from_sources(0b0101));

    assert!(event.handled());
    assert_eq!(event.sources(), 0b0101);
}
