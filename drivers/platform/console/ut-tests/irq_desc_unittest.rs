// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kirq::IrqSource;
use unittest::{assert_eq, def_test};

use super::*;

/// `device_tree_irq_desc` must tag every parsed interrupt as device-tree
/// sourced and propagate the controller-local IRQ number. (It deliberately
/// does not over-assert the trigger/polarity encoding, which is owned by
/// the `kirq` constructors.)
#[def_test]
fn device_tree_irq_desc_tags_source_and_propagates_hwirq() {
    let gic = of::InterruptInfo {
        irq: 33,
        trigger: of::InterruptTrigger::EdgeRising,
        controller: of::InterruptControllerKind::Gic,
    };
    let desc = device_tree_irq_desc(gic);
    assert_eq!(desc.source, IrqSource::DeviceTree);
    assert_eq!(desc.hwirq, 33);

    let plic = of::InterruptInfo {
        irq: 16,
        trigger: of::InterruptTrigger::LevelHigh,
        controller: of::InterruptControllerKind::Plic,
    };
    let desc = device_tree_irq_desc(plic);
    assert_eq!(desc.source, IrqSource::DeviceTree);
    assert_eq!(desc.hwirq, 16);
}
