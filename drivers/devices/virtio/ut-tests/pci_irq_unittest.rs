// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI fallback routing tests with no configuration-space or IRQ hardware access.

use driver_os_api::{IrqController, IrqResource, IrqTrigger, ResError, Virq};
use unittest::{assert_eq, def_test};

use super::loongarch_legacy_irq;

#[def_test]
fn loongarch_intx_maps_each_slot_and_returns_provider_virq() {
    for device in 0..32u8 {
        let mut offered = None;
        let virq = 0x2000 + usize::from(device);
        let result = loongarch_legacy_irq(device, |desc| {
            offered = Some(desc);
            Ok(IrqResource::plain(Virq::new(virq), desc.trigger))
        });
        let desc = offered.unwrap();
        assert_eq!(desc.hwirq, 0x10 + usize::from(device % 4));
        assert_eq!(desc.controller, IrqController::LoongArchExtioi);
        assert_eq!(desc.trigger, IrqTrigger::LevelHigh);
        assert_eq!(desc.domain, None);
        assert_eq!(result, Some(virq));
    }
}

#[def_test]
fn loongarch_intx_mapping_failure_returns_no_irq() {
    let mut calls = 0;
    let result = loongarch_legacy_irq(3, |_| {
        calls += 1;
        Err(ResError::Unsupported)
    });
    assert_eq!(calls, 1);
    assert_eq!(result, None);
}
