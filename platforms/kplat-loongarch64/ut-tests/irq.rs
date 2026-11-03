// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn local_sources_do_not_configure_external_pins() {
    for irq in [super::TIMER_IRQ, super::EIOINTC_IRQ] {
        assert_eq!(super::configure_with(kirq::IrqDesc::from_virq(irq), |_, _| {
            panic!("CPU-local source accessed PCH configuration")
        }), Ok(()));
    }
    let desc = kirq::IrqDesc::new(17, kirq::IrqTrigger::LevelLow);
    let mut calls = 0;
    assert_eq!(super::configure_with(desc, |irq, is_low| {
        core::assert_eq!(irq, 17);
        core::assert!(is_low);
        calls += 1;
        true
    }), Ok(()));
    assert_eq!(calls, 1);
    assert_eq!(super::configure_with(desc, |_, _| false),
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: 17 }));
}

#[def_test]
fn external_interrupt_parent_is_enabled_after_boot() {
    // Read boot's persistent gate without changing live interrupt state.
    assert!(
        super::ecfg::read()
            .lie()
            .contains(super::LineBasedInterrupt::HWI1)
    );
}

#[def_test]
fn external_level_selection_rejects_unimplemented_edge_and_routing() {
    use kirq::{IrqController, IrqDesc, IrqFlow, IrqTrigger};
    for trigger in [IrqTrigger::LevelHigh, IrqTrigger::LevelLow, IrqTrigger::Unknown(0)] {
        let desc = IrqDesc::new(17, trigger).with_controller(IrqController::LoongArchExtioi);
        assert!(matches!(super::select_flow(desc), Ok(IrqFlow::Level(_))));
    }
    for trigger in [IrqTrigger::EdgeRising, IrqTrigger::EdgeFalling] {
        assert!(super::select_flow(IrqDesc::new(17, trigger)).is_err());
    }
    for irq in [3, 11, 64, usize::MAX] {
        let desc = IrqDesc::new(irq, IrqTrigger::LevelHigh).with_controller(IrqController::LoongArchExtioi);
        assert!(super::select_flow(desc).is_err());
    }
    for irq in [3, 11] {
        assert_eq!(super::select_flow(IrqDesc::from_virq(irq)),
            Ok(if irq == 11 { IrqFlow::PerCpu } else { IrqFlow::Legacy }));
    }
    let desc = IrqDesc::new(17, IrqTrigger::LevelHigh);
    assert!(super::select_flow(desc.with_affinity(kirq::IrqAffinity::Cpu(1))).is_err());
    assert!(super::select_flow(desc.with_flags(kirq::IrqFlags::PER_CPU)).is_err());
}

#[def_test]
fn parent_claim_is_consumed_before_leaf_dispatch_including_rejected_vectors() {
    for irq in [0, 17, 63, 3, 11, 64, 255] {
        let mut acknowledgements = 0;
        let result = super::claim_external(|| Some(irq), |claimed| {
            core::assert_eq!(claimed, irq);
            acknowledgements += 1;
        });
        assert_eq!(acknowledgements, 1);
        assert_eq!(result, if irq < 64 && irq != 3 && irq != 11 { Some(irq) } else { None });
    }
    assert_eq!(super::claim_external(|| None, |_| panic!("spurious claim completed")), None);
}
