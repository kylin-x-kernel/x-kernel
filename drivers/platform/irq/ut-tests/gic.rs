// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

#[def_test]
fn standard_intid_limit_uses_lines_not_id_bits() {
    for id_bits in [0, 7, 15, 31] {
        let other_fields = id_bits << 19;
        assert_eq!(super::standard_intid_limit(other_fields), 32);
        assert_eq!(super::standard_intid_limit(other_fields | 1), 64);
        assert_eq!(super::standard_intid_limit(other_fields | 30), 992);
        assert_eq!(super::standard_intid_limit(other_fields | 31), 1020);
    }
}

#[def_test]
fn explicit_gic_sources_require_discovered_standard_range() {
    use kirq::{IrqController, IrqDesc, IrqDescError, IrqFlow};

    for hwirq in [0, 15, 16, 31, 32, 63] {
        for desc in [
            IrqDesc::from_hwirq(hwirq).with_controller(IrqController::Gic),
            IrqDesc::from_hwirq(hwirq).with_domain(super::GIC_ROOT_DOMAIN),
        ] {
            let expected = if hwirq < 32 { IrqFlow::PerCpu }
                else { IrqFlow::Fasteoi(&super::spi::CHIP) };
            assert_eq!(super::select_gic_flow(desc, Some(64)), Ok(expected));
            assert_eq!(super::select_gic_flow(desc, None),
                Err(IrqDescError::UnsupportedFlow { hwirq }));
        }
    }
    for (limit, hwirq) in [
        (32, 32), (64, 64), (992, 992), (1020, 1020), (1020, 1023),
        (1020, 1024), (1020, 8192), (1020, u32::MAX as usize + 1),
        (1020, usize::MAX),
    ] {
        let desc = super::level_irq_desc(hwirq);
        assert_eq!(super::select_gic_flow(desc, Some(limit)),
            Err(IrqDescError::UnsupportedFlow { hwirq }));
    }
    assert_eq!(super::select_gic_flow(super::level_irq_desc(1019), Some(1020)),
        Ok(IrqFlow::Fasteoi(&super::spi::CHIP)));
}

#[def_test]
fn gic_identity_must_not_name_another_controller_or_domain() {
    use kirq::{IrqController, IrqDesc, IrqDescError, IrqFlow};

    let hwirq = 32;
    for desc in [
        super::level_irq_desc(hwirq).with_controller(IrqController::Plic),
        super::level_irq_desc(hwirq).with_domain(kirq::PLIC_ROOT_DOMAIN),
    ] {
        assert_eq!(super::select_gic_flow(desc, Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq }));
    }
    // This gate must not reinterpret software-only fixtures or other backends.
    assert_eq!(super::select_gic_flow(IrqDesc::from_virq(0x6000), None),
        Ok(IrqFlow::Legacy));
}

#[def_test]
fn explicit_gic_spis_reject_unsupported_trigger_polarity() {
    use kirq::{IrqController, IrqDesc, IrqDescError, IrqFlow, IrqTrigger};

    for hwirq in [32, 63] {
        for trigger in [IrqTrigger::EdgeFalling, IrqTrigger::LevelLow] {
            for desc in [
                IrqDesc::new(hwirq, trigger).with_controller(IrqController::Gic),
                IrqDesc::new(hwirq, trigger).with_domain(super::GIC_ROOT_DOMAIN),
            ] {
                assert_eq!(super::select_gic_flow(desc, Some(64)),
                    Err(IrqDescError::UnsupportedFlow { hwirq }));
            }
        }
        for trigger in [IrqTrigger::EdgeRising, IrqTrigger::LevelHigh,
                        IrqTrigger::Unknown(0), IrqTrigger::Unknown(0x100)] {
            assert_eq!(super::select_gic_flow(super::irq_desc(hwirq, trigger), Some(64)),
                Ok(IrqFlow::Fasteoi(&super::spi::CHIP)));
        }
    }
    // PPI polarity can be fixed by the integration. Do not impose the SPI
    // policy on private sources or reinterpret undeclared legacy resources.
    for hwirq in [0, 15, 16, 31] {
        assert_eq!(super::select_gic_flow(
            super::irq_desc(hwirq, IrqTrigger::LevelLow), Some(64)), Ok(IrqFlow::PerCpu));
    }
    assert_eq!(super::select_gic_flow(IrqDesc::new(32, IrqTrigger::EdgeFalling), None),
        Ok(IrqFlow::Legacy));
}

#[def_test]
fn spi_flow_excludes_private_nmi_and_unsupported_routing() {
    use kirq::{IrqAffinity, IrqDescError, IrqFlags, IrqFlow, IrqPolarity};
    for hwirq in [0, 15, 16, 27, 31] {
        assert_eq!(super::select_gic_flow(super::edge_irq_desc(hwirq), Some(64)),
            Ok(if hwirq == 27 { IrqFlow::Legacy } else { IrqFlow::PerCpu }));
    }
    // register_nmi tags its fallback PER_CPU before flow selection, including
    // SPI metadata. This is not an ordinary-source hardware capability.
    let spi = super::edge_irq_desc(32);
    assert_eq!(super::select_gic_flow(spi.with_flags(IrqFlags::PER_CPU), Some(64)),
        Ok(IrqFlow::Legacy));
    for flags in [IrqFlags::empty(), IrqFlags::SHARED, IrqFlags::WAKEUP_SOURCE,
                  IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE] {
        assert_eq!(super::select_gic_flow(spi.with_flags(flags), Some(64)),
            Ok(IrqFlow::Fasteoi(&super::spi::CHIP)));
    }
    for desc in [spi.with_flags(IrqFlags::MSI),
                 spi.with_flags(IrqFlags::from_bits_retain(1 << 31)),
                 kirq::IrqDesc { polarity: IrqPolarity::Low, ..spi },
                 kirq::IrqDesc { affinity: IrqAffinity::Cpu(0), ..spi }] {
        assert_eq!(super::select_gic_flow(desc, Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq: 32 }));
    }
    assert_eq!(matches!(super::spi::CHIP.resend, kirq::FasteoiResend::HardwareOnly(_)), true);
}

#[def_test]
fn repeated_local_initialization_is_rejected_before_hardware_setup() {
    // Kernel tests run after this CPU's mandatory controller preparation.
    // Keep the current CPU and mask stable while testing the rejection path.
    let _guard = kspin::NoPreemptIrqSave::new();
    #[cfg(feature = "nmi-pseudo")]
    let saved_pmr = karch::pmr::is_ready().then(karch::pmr::read);

    let result = super::init_current_cpu();
    assert_eq!(result, Err("local GIC initialization already attempted"));
    assert_eq!(karch::local_irq_enabled(), false);
    #[cfg(feature = "nmi-pseudo")]
    if let Some(saved) = saved_pmr {
        assert_eq!(karch::pmr::read(), saved);
    }
}
