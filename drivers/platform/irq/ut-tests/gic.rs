// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn guest_interface_requires_disjoint_firmware_virtualization_pages() {
    use super::{GicConfig, GicMmioRegion, GicVersion};
    let region = |address, size| GicMmioRegion {
        paddr: memaddr::PhysAddr::from_usize(address),
        size,
    };
    let mut config = GicConfig {
        version: GicVersion::V2,
        gicd: region(0x08000000, 0x10000),
        gicc: Some(region(0x08010000, 0x10000)),
        gicr: None,
        gich: None,
        gicv: None,
    };
    assert_eq!(super::guest_interface(config), None);
    config.gich = Some(region(0x08030000, 0x10000));
    assert_eq!(super::guest_interface(config), None);
    config.gicv = Some(region(0x08040000, 0x10000));
    assert_eq!(
        super::guest_interface(config),
        Some(kirq::GuestIrqInterface::GicV2 {
            control_paddr: 0x08030000,
            cpu_paddr: 0x08040000,
        })
    );
    for invalid in [
        region(0x08030000, 0x2000),
        region(0x08040001, 0x2000),
        region(0x08040000, 0x1000),
        region(usize::MAX & !0xfff, 0x2000),
    ] {
        config.gicv = Some(invalid);
        assert_eq!(super::guest_interface(config), None);
    }
    config.gicv = Some(region(0x08040000, 0x2000));
    config.version = GicVersion::V3;
    assert_eq!(super::guest_interface(config), None);
}

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
            let expected = if hwirq < 32 {
                IrqFlow::PerCpu
            } else {
                IrqFlow::Fasteoi(&super::spi::CHIP)
            };
            assert_eq!(super::select_gic_flow(desc, Some(64)), Ok(expected));
            assert_eq!(
                super::select_gic_flow(desc, None),
                Err(IrqDescError::UnsupportedFlow { hwirq })
            );
        }
    }
    for (limit, hwirq) in [
        (32, 32),
        (64, 64),
        (992, 992),
        (1020, 1020),
        (1020, 1023),
        (1020, 1024),
        (1020, 8192),
        (1020, u32::MAX as usize + 1),
        (1020, usize::MAX),
    ] {
        let desc = super::level_irq_desc(hwirq);
        assert_eq!(
            super::select_gic_flow(desc, Some(limit)),
            Err(IrqDescError::UnsupportedFlow { hwirq })
        );
    }
    assert_eq!(
        super::select_gic_flow(super::level_irq_desc(1019), Some(1020)),
        Ok(IrqFlow::Fasteoi(&super::spi::CHIP))
    );
}

#[def_test]
fn gic_identity_must_not_name_another_controller_or_domain() {
    use kirq::{IrqController, IrqDesc, IrqDescError};

    let hwirq = 32;
    for desc in [
        super::level_irq_desc(hwirq).with_controller(IrqController::Plic),
        super::level_irq_desc(hwirq).with_domain(kirq::PLIC_ROOT_DOMAIN),
        kirq::plic_irq_desc(hwirq),
        IrqDesc::from_hwirq(hwirq),
    ] {
        assert_eq!(
            super::select_gic_flow(desc, Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq })
        );
    }
    assert_eq!(
        super::select_gic_flow(IrqDesc::from_virq(0x6000), None),
        Err(IrqDescError::UnsupportedFlow { hwirq: 0x6000 })
    );
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
                assert_eq!(
                    super::select_gic_flow(desc, Some(64)),
                    Err(IrqDescError::UnsupportedFlow { hwirq })
                );
            }
        }
        for trigger in [
            IrqTrigger::EdgeRising,
            IrqTrigger::LevelHigh,
            IrqTrigger::Unknown(0),
            IrqTrigger::Unknown(0x100),
        ] {
            assert_eq!(
                super::select_gic_flow(super::irq_desc(hwirq, trigger), Some(64)),
                Ok(IrqFlow::Fasteoi(&super::spi::CHIP))
            );
        }
    }
    // PPI polarity can be fixed by the integration. Do not impose the SPI
    // policy on private sources.
    for hwirq in [0, 15, 16, 31] {
        assert_eq!(
            super::select_gic_flow(super::irq_desc(hwirq, IrqTrigger::LevelLow), Some(64)),
            Ok(IrqFlow::PerCpu)
        );
    }
    assert_eq!(
        super::select_gic_flow(IrqDesc::new(32, IrqTrigger::EdgeFalling), None),
        Err(IrqDescError::UnsupportedFlow { hwirq: 32 })
    );
}

#[def_test]
fn spi_flow_excludes_private_nmi_and_unsupported_routing() {
    use kirq::{IrqAffinity, IrqDescError, IrqFlags, IrqFlow, IrqPolarity};
    for hwirq in [0, 15, 16, 27, 31] {
        assert_eq!(
            super::select_gic_flow(super::edge_irq_desc(hwirq), Some(64)),
            Ok(IrqFlow::PerCpu)
        );
    }
    // SGI15 irq_work and PMU PPIs must use the claiming CPU's enable state,
    // regardless of whether registration explicitly supplied PER_CPU.
    for hwirq in [0, 15, 16, 23, 27, 31] {
        assert_eq!(
            super::select_gic_flow(
                super::level_irq_desc(hwirq).with_flags(IrqFlags::PER_CPU),
                Some(64),
            ),
            Ok(IrqFlow::PerCpu)
        );
    }
    let spi = super::edge_irq_desc(32);
    for hwirq in [32, 63, 1019, 1020] {
        assert_eq!(
            super::select_gic_flow(
                super::edge_irq_desc(hwirq).with_flags(IrqFlags::PER_CPU),
                Some(1020)
            ),
            Err(IrqDescError::UnsupportedFlow { hwirq })
        );
    }
    for flags in [
        IrqFlags::empty(),
        IrqFlags::SHARED,
        IrqFlags::WAKEUP_SOURCE,
        IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE,
    ] {
        assert_eq!(
            super::select_gic_flow(spi.with_flags(flags), Some(64)),
            Ok(IrqFlow::Fasteoi(&super::spi::CHIP))
        );
    }
    for desc in [
        spi.with_flags(IrqFlags::MSI),
        spi.with_flags(IrqFlags::from_bits_retain(1 << 31)),
        kirq::IrqDesc {
            polarity: IrqPolarity::Low,
            ..spi
        },
        kirq::IrqDesc {
            affinity: IrqAffinity::Cpu(0),
            ..spi
        },
    ] {
        assert_eq!(
            super::select_gic_flow(desc, Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq: 32 })
        );
    }
    assert_eq!(
        matches!(
            super::spi::CHIP.resend,
            kirq::FasteoiResend::HardwareOnly(_)
        ),
        true
    );
}

#[def_test]
fn raw_private_sources_use_local_control_without_granting_special_capabilities() {
    use kirq::{IrqDesc, IrqFlags, IrqFlow};

    for hwirq in [0, 15, 16, 27, 31] {
        for flags in [IrqFlags::empty(), IrqFlags::PER_CPU] {
            let desc = IrqDesc::from_hwirq(hwirq).with_flags(flags);
            assert_eq!(super::select_gic_flow(desc, None), Ok(IrqFlow::PerCpu));
            assert!(kirq::IntrManagerIf::percpu_nmi(desc).is_none());
            assert!(kirq::IntrManagerIf::percpu_forwarding(desc).is_none());
        }
    }
    for hwirq in [0, 15] {
        let desc = super::edge_irq_desc(hwirq).with_flags(IrqFlags::PER_CPU);
        assert_eq!(super::select_gic_flow(desc, Some(64)), Ok(IrqFlow::PerCpu));
        assert!(kirq::IntrManagerIf::percpu_nmi(desc).is_none());
        assert!(kirq::IntrManagerIf::percpu_forwarding(desc).is_none());
    }
    // On each supported backend, a real PPI must still expose its capability;
    // the negative SGI checks above must not pass merely because all are absent.
    match super::active_version() {
        super::GicVersion::V2 => {
            assert!(kirq::IntrManagerIf::percpu_forwarding(super::level_irq_desc(27)).is_some())
        }
        super::GicVersion::V3 => {
            #[cfg(any(feature = "nmi-pseudo", feature = "nmi-hardware"))]
            if kplat::nm_irq::NmiMechanismIf::mode() != kplat::nm_irq::NmiMode::None {
                assert!(kirq::IntrManagerIf::percpu_nmi(super::level_irq_desc(23)).is_some());
            }
        }
    }
}

struct ForeignBackend;

impl fwnode::FwNodeOps<'static> for ForeignBackend {
    fn source(&self) -> fwnode::FwSource {
        fwnode::FwSource::Static
    }

    fn name(&self, _: fwnode::FwNodeId) -> Option<&'static str> {
        None
    }

    fn parent(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn first_child(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn next_sibling(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn is_available(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(true)
    }

    fn is_interrupt_controller(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(false)
    }

    fn firmware_id(&self, _: fwnode::FwNodeId, _: usize) -> Option<&'static str> {
        None
    }

    fn property(
        &self,
        _: fwnode::FwNodeId,
        _: &str,
    ) -> Result<Option<fwnode::FwProperty<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn mmio_resource(
        &self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwMmioResource<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn irq_spec(
        &'static self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwIrqSpec<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn reference(
        &'static self,
        _: fwnode::FwNodeId,
        _: &str,
        _: usize,
        _: usize,
    ) -> Result<Option<fwnode::FwReference<'static>>, fwnode::FwError> {
        Ok(None)
    }
}

static FOREIGN_BACKEND: ForeignBackend = ForeignBackend;

struct ForeignXlate;

impl kirq::IrqDomainXlate for ForeignXlate {
    fn translate(&self, _: &[u32]) -> Result<kirq::IrqHwSpec, kirq::IrqXlateError> {
        Err(kirq::IrqXlateError::Unsupported)
    }
}

static FOREIGN_XLATE: ForeignXlate = ForeignXlate;

#[def_test(serial)]
fn another_gic_instance_domain_is_not_admitted_by_this_driver() {
    use kirq::{IrqController, IrqDescError};

    // Force a dynamically allocated domain: this driver owns only the
    // singleton GIC whose identifier `gic_domain()` resolves, not every domain
    // whose metadata says "GIC".
    let controller = fwnode::FwNode::new(&FOREIGN_BACKEND, fwnode::FwNodeId::new(1));
    let foreign = kirq::register_irq_domain(
        kirq::IrqDomainTarget::New(IrqController::Gic),
        controller,
        &FOREIGN_XLATE,
    )
    .expect("foreign GIC instance registers its own domain");
    assert_ne!(foreign, super::gic_domain());

    for hwirq in [16, 32, 63] {
        assert_eq!(
            super::select_gic_flow(super::level_irq_desc(hwirq).with_domain(foreign), Some(64)),
            Err(IrqDescError::UnsupportedFlow { hwirq })
        );
    }
}

#[def_test]
fn repeated_local_initialization_is_rejected_before_hardware_setup() {
    // Kernel tests run after this CPU's mandatory controller preparation.
    // Keep the current CPU and mask stable while testing the rejection path.
    let _guard = kspin::NoPreemptIrqSave::new();
    #[cfg(feature = "nmi-pseudo")]
    let saved_pmr = karch::pmr::is_ready().then(karch::pmr::read);

    let result = super::init_current_cpu();
    assert_eq!(result, Err(super::GicInitError::LocalInitAlreadyAttempted));
    assert_eq!(karch::local_irq_enabled(), false);
    #[cfg(feature = "nmi-pseudo")]
    if let Some(saved) = saved_pmr {
        assert_eq!(karch::pmr::read(), saved);
    }
}
