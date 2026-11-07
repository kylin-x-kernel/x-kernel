// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use fwnode::{
    FwError, FwIrqSpec, FwMmioResource, FwNode, FwNodeId, FwNodeOps, FwParams, FwProperty,
    FwReference, FwSource,
};
use unittest::{assert_eq, def_test};

use crate::{
    FwIrqMapError, IrqController, IrqDesc, IrqDescError, IrqDomainId, IrqDomainRegisterError,
    IrqDomainTarget, IrqDomainXlate, IrqHwSpec, IrqPolarity, IrqTrigger, IrqXlateError,
};

struct TestBackend {
    // Distinct values keep the two static backends at distinct addresses;
    // zero-sized statics may legally be merged, which would erase identity.
    tag: u8,
}

impl FwNodeOps<'static> for TestBackend {
    fn source(&self) -> FwSource {
        FwSource::Static
    }

    fn name(&self, _: FwNodeId) -> Option<&'static str> {
        Some(match self.tag {
            1 => "test-interrupt-controller",
            2 => "other-interrupt-controller",
            3 => "second-instance-interrupt-controller",
            _ => "root-interrupt-controller",
        })
    }

    fn parent(&self, _: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn first_child(&self, _: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn next_sibling(&self, _: FwNodeId) -> Option<FwNodeId> {
        None
    }

    fn is_available(&self, _: FwNodeId) -> Result<bool, FwError> {
        Ok(true)
    }

    fn is_interrupt_controller(&self, _: FwNodeId) -> Result<bool, FwError> {
        Ok(false)
    }

    fn firmware_id(&self, _: FwNodeId, _: usize) -> Option<&'static str> {
        None
    }

    fn property(&self, _: FwNodeId, _: &str) -> Result<Option<FwProperty<'static>>, FwError> {
        Ok(None)
    }

    fn mmio_resource(
        &self,
        _: FwNodeId,
        _: usize,
    ) -> Result<Option<FwMmioResource<'static>>, FwError> {
        Ok(None)
    }

    fn irq_spec(&'static self, _: FwNodeId, _: usize) -> Result<Option<FwIrqSpec<'static>>, FwError> {
        Ok(None)
    }

    fn reference(
        &'static self,
        _: FwNodeId,
        _: &str,
        _: usize,
        _: usize,
    ) -> Result<Option<FwReference<'static>>, FwError> {
        Ok(None)
    }
}

static TEST_BACKEND: TestBackend = TestBackend { tag: 1 };
static OTHER_BACKEND: TestBackend = TestBackend { tag: 2 };
static SECOND_INSTANCE_BACKEND: TestBackend = TestBackend { tag: 3 };
static ROOT_BACKEND: TestBackend = TestBackend { tag: 4 };

const TEST_NODE: FwNodeId = FwNodeId::new(1);

static TEST_CONTROLLER: FwNode<'static> = FwNode::new(&TEST_BACKEND, TEST_NODE);
// The same backend-local id in another backend is a different controller.
static OTHER_CONTROLLER: FwNode<'static> = FwNode::new(&OTHER_BACKEND, TEST_NODE);
// A second controller of the same family remains a distinct firmware node.
static SECOND_INSTANCE_CONTROLLER: FwNode<'static> =
    FwNode::new(&SECOND_INSTANCE_BACKEND, TEST_NODE);
static ROOT_CONTROLLER: FwNode<'static> = FwNode::new(&ROOT_BACKEND, TEST_NODE);
static OTHER_ROOT_CONTROLLER: FwNode<'static> = FwNode::new(&ROOT_BACKEND, FwNodeId::new(2));

struct TestXlate;

impl IrqDomainXlate for TestXlate {
    fn translate(&self, params: &[u32]) -> Result<IrqHwSpec, IrqXlateError> {
        match params {
            [hwirq, 1] => Ok(IrqHwSpec {
                hwirq: *hwirq as usize,
                trigger: IrqTrigger::EdgeRising,
            }),
            [hwirq, 4] => Ok(IrqHwSpec {
                hwirq: *hwirq as usize,
                trigger: IrqTrigger::LevelHigh,
            }),
            [_, _] => Err(IrqXlateError::Unsupported),
            _ => Err(IrqXlateError::Malformed),
        }
    }
}

static TEST_XLATE: TestXlate = TestXlate;

fn spec(params: &[u32]) -> FwIrqSpec<'static> {
    FwIrqSpec {
        controller: TEST_CONTROLLER,
        params: FwParams::new(params).unwrap(),
    }
}

#[def_test(serial)]
fn descriptor_merge_adopts_concrete_metadata_and_rejects_conflicts() {
    let base = IrqDesc::new(7, IrqTrigger::Unknown(0));
    let merged = base
        .try_merge(IrqDesc::new(7, IrqTrigger::LevelHigh))
        .expect("unknown trigger adopts a concrete declaration");
    assert_eq!(merged.trigger, IrqTrigger::LevelHigh);
    assert_eq!(merged.polarity, IrqPolarity::High);

    assert_eq!(
        merged
            .try_merge(IrqDesc::new(7, IrqTrigger::EdgeRising))
            .expect_err("a concrete trigger cannot be replaced"),
        IrqDescError::TriggerConflict {
            existing: IrqTrigger::LevelHigh,
            newer: IrqTrigger::EdgeRising,
        }
    );
    let high = IrqDesc::new(7, IrqTrigger::LevelHigh);
    assert_eq!(
        high.try_merge(high.with_polarity(IrqPolarity::Low))
            .expect_err("a concrete polarity cannot be replaced"),
        IrqDescError::PolarityConflict {
            existing: IrqPolarity::High,
            newer: IrqPolarity::Low,
        }
    );
    let gic = high.with_controller(IrqController::Gic);
    assert_eq!(
        gic.try_merge(gic.with_controller(IrqController::Plic))
            .expect_err("a concrete controller cannot be replaced"),
        IrqDescError::ControllerConflict {
            existing: IrqController::Gic,
            newer: IrqController::Plic,
        }
    );
}

#[def_test(serial)]
fn existing_domain_association_is_explicit_and_exclusive() {
    let domain = crate::domain::SECOND_TEST_DOMAIN;
    let unassigned = IrqDomainId::new(0x7fff_fffe);
    assert_eq!(
        crate::register_irq_domain(
            IrqDomainTarget::Existing(unassigned),
            ROOT_CONTROLLER,
            &TEST_XLATE,
        )
        .expect_err("an unknown existing domain is rejected"),
        IrqDomainRegisterError::UnknownDomain { domain: unassigned }
    );

    assert_eq!(
        crate::register_irq_domain(
            IrqDomainTarget::Existing(domain),
            ROOT_CONTROLLER,
            &TEST_XLATE,
        )
        .expect("boot wiring explicitly selects an existing domain"),
        domain
    );
    let binding = crate::domain::fw::lookup(ROOT_CONTROLLER).expect("root controller associated");
    assert_eq!(binding.domain, domain);
    assert_eq!(
        binding.kind,
        IrqController::Plic,
        "an existing domain keeps its own family metadata"
    );
    assert!(!crate::domain::domain(domain)
        .expect("existing domain remains registered")
        .is_identity());

    assert_eq!(
        crate::register_irq_domain(
            IrqDomainTarget::Existing(domain),
            OTHER_ROOT_CONTROLLER,
            &TEST_XLATE,
        )
        .expect_err("an existing domain accepts only one controller"),
        IrqDomainRegisterError::DomainAlreadyRegistered { domain }
    );
}

#[def_test(serial)]
fn firmware_domain_registration_and_translation_use_exact_node_identity() {
    let domain = crate::register_irq_domain(
        IrqDomainTarget::New(IrqController::Gic),
        TEST_CONTROLLER,
        &TEST_XLATE,
    )
    .expect("first association succeeds");

    let binding = crate::domain::fw::lookup(TEST_CONTROLLER).expect("controller is associated");
    assert_eq!(binding.domain, domain);
    assert_eq!(binding.kind, IrqController::Gic);
    assert_eq!(crate::domain::domain_kind(domain), Some(IrqController::Gic));
    assert_eq!(
        binding.xlate.translate(&[42, 4]),
        Ok(IrqHwSpec {
            hwirq: 42,
            trigger: IrqTrigger::LevelHigh,
        })
    );
    assert_eq!(binding.xlate.translate(&[42]), Err(IrqXlateError::Malformed));
    assert_eq!(
        binding.xlate.translate(&[42, 2]),
        Err(IrqXlateError::Unsupported)
    );

    assert_eq!(
        crate::register_irq_domain(
            IrqDomainTarget::New(IrqController::Gic),
            TEST_CONTROLLER,
            &TEST_XLATE,
        )
        .expect_err("a controller registers once"),
        IrqDomainRegisterError::ControllerAlreadyRegistered {
            controller: TEST_NODE,
            existing: domain,
        }
    );

    // A second controller of the same family gets its own namespace instead of
    // colliding with the first instance's domain.
    let second = crate::register_irq_domain(
        IrqDomainTarget::New(IrqController::Gic),
        SECOND_INSTANCE_CONTROLLER,
        &TEST_XLATE,
    )
    .expect("second instance of the same family registers");
    assert_ne!(second, domain);
    let second_binding =
        crate::domain::fw::lookup(SECOND_INSTANCE_CONTROLLER).expect("second controller associated");
    assert_eq!(second_binding.domain, second);
    assert_eq!(second_binding.kind, IrqController::Gic);

    // Numeric identity fallback belongs to the reserved root. A newly allocated
    // instance must require an explicit mapping instead of resolving a bare
    // hardware number to an unrelated global logical number.
    assert!(!crate::domain::domain(second).expect("second domain").is_identity());
    assert_eq!(crate::domain::resolve(second, 42), None);

    // Numeric identity alone must not associate a node from another backend.
    assert!(crate::domain::fw::lookup(OTHER_CONTROLLER).is_none());
    assert_eq!(
        crate::try_map_fwspec(FwIrqSpec {
            controller: OTHER_CONTROLLER,
            params: FwParams::new(&[42, 4]).unwrap(),
        })
        .expect_err("unregistered controller is not ready"),
        FwIrqMapError::ControllerNotReady
    );
    assert_eq!(
        crate::try_map_fwspec(spec(&[42]))
            .expect_err("malformed binding cells are rejected before mapping"),
        FwIrqMapError::MalformedSpec
    );
    assert_eq!(
        crate::try_map_fwspec(spec(&[42, 2]))
            .expect_err("unsupported binding cells are rejected before mapping"),
        FwIrqMapError::UnsupportedSpec
    );

    // The platform's flow policy rejects synthetic domains, so publish the
    // source first with unknown metadata and let the firmware declaration
    // merge into that identity. This still exercises lookup-by-domain, the
    // controller's translation, and adoption of binding-derived metadata.
    let existing = crate::runtime::manager::tests_irq::SyntheticLine::mapped(
        IrqDesc::from_hwirq(42).with_domain(domain),
    );
    let first = crate::try_map_fwspec(spec(&[42, 4])).expect("valid declaration maps");
    assert_eq!(first.virq, existing.virq());
    assert_eq!(first.desc.trigger, IrqTrigger::LevelHigh);
    // The controller family comes from the domain, not from the caller.
    assert_eq!(first.desc.controller, IrqController::Gic);
    let repeated = crate::try_map_fwspec(spec(&[42, 4])).expect("same declaration reuses mapping");
    assert_eq!(repeated.virq, first.virq);

    // A second declaration for the same source with a different trigger is a
    // conflict, not a silent metadata update.
    assert_eq!(
        crate::try_map_fwspec(spec(&[42, 1])).expect_err("conflicting trigger is rejected"),
        FwIrqMapError::Map(IrqDescError::TriggerConflict {
            existing: IrqTrigger::LevelHigh,
            newer: IrqTrigger::EdgeRising,
        })
    );
    let retained = crate::descriptor(first.virq).expect("original mapping remains");
    assert_eq!(retained.trigger, IrqTrigger::LevelHigh);
    assert_eq!(retained.hwirq, 42);
    assert_eq!(
        crate::try_map_fwspec(spec(&[42, 4]))
            .expect("repeating the original declaration still succeeds")
            .virq,
        first.virq
    );

    // The same hardware number in a second instance is a distinct source. It
    // must not inherit the first instance's mapping or metadata.
    assert_eq!(
        crate::domain::resolve(second, 42),
        None,
        "an unmapped instance must not borrow the first instance's mapping"
    );
    let second_existing = crate::runtime::manager::tests_irq::SyntheticLine::mapped(
        IrqDesc::from_hwirq(42).with_domain(second),
    );
    assert_ne!(second_existing.virq(), first.virq);
    let second_mapped = crate::try_map_fwspec(FwIrqSpec {
        controller: SECOND_INSTANCE_CONTROLLER,
        params: FwParams::new(&[42, 1]).unwrap(),
    })
    .expect("second instance maps its own source");
    assert_eq!(second_mapped.virq, second_existing.virq());
    assert_eq!(second_mapped.desc.domain, Some(second));
    assert_eq!(second_mapped.desc.controller, IrqController::Gic);
    assert_eq!(second_mapped.desc.trigger, IrqTrigger::EdgeRising);
    assert_eq!(
        crate::domain::resolve(second, 42),
        Some(second_existing.virq())
    );
    assert_eq!(crate::domain::resolve(domain, 42), Some(first.virq));

    // A metadata conflict in one instance leaves the other instance intact.
    assert_eq!(
        crate::try_map_fwspec(FwIrqSpec {
            controller: SECOND_INSTANCE_CONTROLLER,
            params: FwParams::new(&[42, 4]).unwrap(),
        })
        .expect_err("conflicting trigger in the second instance is rejected"),
        FwIrqMapError::Map(IrqDescError::TriggerConflict {
            existing: IrqTrigger::EdgeRising,
            newer: IrqTrigger::LevelHigh,
        })
    );
    let first_retained = crate::descriptor(first.virq).expect("first mapping remains");
    assert_eq!(first_retained.trigger, IrqTrigger::LevelHigh);
    assert_eq!(first_retained.hwirq, 42);

    // Removing the second instance's line withdraws only its own index entry.
    drop(second_existing);
    assert_eq!(crate::domain::resolve(second, 42), None);
    assert_eq!(crate::domain::resolve(domain, 42), Some(first.virq));
    assert_eq!(
        crate::try_map_fwspec(spec(&[42, 4]))
            .expect("first declaration still maps after the second is released")
            .virq,
        first.virq
    );
}
