// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;

use super::{
    Affinity, IntId, IrqAffinityApplyError, IrqAffinityDelivery, KCpuMask, KCpuMaskExt,
    LogicalCpuId, Registers, apply_locked,
};
use unittest::{assert_eq, def_test};

const INTID: IntId = IntId::spi(17);
const TARGET: Affinity = Affinity {
    aff0: 3,
    aff1: 2,
    aff2: 1,
    aff3: 4,
};

#[derive(Debug, Eq, PartialEq)]
enum Access {
    ReadEnable(u32),
    WriteEnable(u32, bool),
    WaitDisable,
    WriteRoute(u32, Affinity),
    ReadRoute(u32),
}

struct Recorder {
    accesses: Vec<Access>,
    is_enabled: bool,
    route: Option<Affinity>,
    has_disable_timeout: bool,
    ignores_mask: bool,
    ignores_route: bool,
    ignores_restore: bool,
}

impl Recorder {
    fn new(is_enabled: bool) -> Self {
        Self {
            accesses: Vec::new(),
            is_enabled,
            route: Some(Affinity::default()),
            has_disable_timeout: false,
            ignores_mask: false,
            ignores_route: false,
            ignores_restore: false,
        }
    }
}

// This narrow interface cannot write priority, trigger or pending state, or
// complete a claim. Exact traces below cover every operation it does expose.
impl Registers for Recorder {
    fn is_enabled(&mut self, intid: IntId) -> bool {
        self.accesses.push(Access::ReadEnable(intid.to_u32()));
        self.is_enabled
    }

    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        self.accesses
            .push(Access::WriteEnable(intid.to_u32(), enabled));
        if !(enabled && self.ignores_restore || !enabled && self.ignores_mask) {
            self.is_enabled = enabled;
        }
    }

    fn wait_for_disable(&mut self) -> bool {
        self.accesses.push(Access::WaitDisable);
        !self.has_disable_timeout
    }

    fn set_route(&mut self, intid: IntId, target: Affinity) {
        self.accesses
            .push(Access::WriteRoute(intid.to_u32(), target));
        if !self.ignores_route {
            self.route = Some(target);
        }
    }

    fn route(&mut self, intid: IntId) -> Option<Affinity> {
        self.accesses.push(Access::ReadRoute(intid.to_u32()));
        self.route
    }
}

fn candidates() -> KCpuMask {
    KCpuMask::one_shot_logical(LogicalCpuId::new(0))
}

#[def_test]
fn source_admission_rejects_private_special_foreign_and_unsupported_sources() {
    let desc = |hwirq| {
        kirq::gic_edge_irq_desc(hwirq).with_domain(crate::gic::gic_domain())
    };
    assert_eq!(super::checked_intid(desc(32), Some(64)), Some(IntId::spi(0)));
    for hwirq in [0, 15, 16, 31, 64, 1019, 1020, usize::MAX] {
        assert_eq!(super::checked_intid(desc(hwirq), Some(64)), None);
    }
    assert_eq!(super::checked_intid(desc(32), None), None);
    for rejected in [
        desc(32).with_domain(kirq::PLIC_ROOT_DOMAIN),
        desc(32).with_controller(kirq::IrqController::Plic),
        desc(32).with_flags(kirq::IrqFlags::PER_CPU),
        desc(32).with_flags(kirq::IrqFlags::MSI),
    ] {
        assert_eq!(super::checked_intid(rejected, Some(64)), None);
    }
}

fn apply(
    registers: &mut Recorder,
    delivery: IrqAffinityDelivery,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    apply_locked(registers, INTID, &candidates(), delivery, |_| Some(TARGET))
}

#[def_test]
fn preserve_enabled_masks_waits_routes_reads_back_then_restores() {
    use Access::*;
    let mut registers = Recorder::new(true);
    assert_eq!(
        apply(&mut registers, IrqAffinityDelivery::Preserve),
        Ok(candidates())
    );
    assert_eq!(registers.is_enabled, true);
    assert_eq!(registers.route, Some(TARGET));
    assert_eq!(
        registers.accesses,
        [
            ReadEnable(49),
            WriteEnable(49, false),
            WaitDisable,
            ReadEnable(49),
            WriteRoute(49, TARGET),
            ReadRoute(49),
            WriteEnable(49, true),
            ReadEnable(49),
        ]
    );
}

#[def_test]
fn preserve_masked_waits_for_disable_without_unmasking() {
    use Access::*;
    let mut registers = Recorder::new(false);
    assert_eq!(
        apply(&mut registers, IrqAffinityDelivery::Preserve),
        Ok(candidates())
    );
    assert_eq!(registers.is_enabled, false);
    assert_eq!(registers.route, Some(TARGET));
    assert_eq!(
        registers.accesses,
        [
            ReadEnable(49),
            WaitDisable,
            ReadEnable(49),
            WriteRoute(49, TARGET),
            ReadRoute(49)
        ]
    );
}

#[def_test]
fn keep_masked_verifies_suppression_for_both_incoming_states() {
    use Access::*;
    for is_enabled in [false, true] {
        let mut registers = Recorder::new(is_enabled);
        assert_eq!(
            apply(&mut registers, IrqAffinityDelivery::KeepMasked),
            Ok(candidates())
        );
        let mut expected = Vec::from([ReadEnable(49)]);
        if is_enabled {
            expected.push(WriteEnable(49, false));
        }
        expected.extend([
            WaitDisable,
            ReadEnable(49),
            WriteRoute(49, TARGET),
            ReadRoute(49),
        ]);
        assert_eq!(registers.accesses, expected);
        assert_eq!(registers.is_enabled, false);
        assert_eq!(registers.route, Some(TARGET));
    }
}

#[def_test]
fn disable_timeout_never_routes_or_restores_delivery() {
    use Access::*;
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        for is_enabled in [false, true] {
            let mut registers = Recorder::new(is_enabled);
            registers.has_disable_timeout = true;
            assert_eq!(
                apply(&mut registers, delivery),
                Err(IrqAffinityApplyError::HardwareFailure)
            );
            let mut expected = Vec::from([ReadEnable(49)]);
            if is_enabled {
                expected.push(WriteEnable(49, false));
            }
            expected.extend([WaitDisable, WriteEnable(49, false)]);
            assert_eq!(registers.accesses, expected);
            assert_eq!(registers.route, Some(Affinity::default()));
            assert_eq!(registers.is_enabled, false);
        }
    }
}

#[def_test]
fn disable_readback_failure_never_routes_or_restores_delivery() {
    use Access::*;
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        let mut registers = Recorder::new(true);
        registers.ignores_mask = true;
        assert_eq!(
            apply(&mut registers, delivery),
            Err(IrqAffinityApplyError::HardwareFailure)
        );
        assert_eq!(
            registers.accesses,
            [
                ReadEnable(49),
                WriteEnable(49, false),
                WaitDisable,
                ReadEnable(49),
                WriteEnable(49, false)
            ]
        );
        // Failure requests masking; it cannot claim that stuck hardware obeyed.
        assert_eq!(registers.is_enabled, true);
        assert_eq!(registers.route, Some(Affinity::default()));
    }
}

#[def_test]
fn route_mismatch_or_missing_readback_never_restores_delivery() {
    use Access::*;
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        for is_enabled in [false, true] {
            for old_route in [None, Some(Affinity::default())] {
                let mut registers = Recorder::new(is_enabled);
                registers.route = old_route;
                registers.ignores_route = true;
                assert_eq!(
                    apply(&mut registers, delivery),
                    Err(IrqAffinityApplyError::HardwareFailure)
                );
                let mut expected = Vec::from([ReadEnable(49)]);
                if is_enabled {
                    expected.push(WriteEnable(49, false));
                }
                expected.extend([
                    WaitDisable,
                    ReadEnable(49),
                    WriteRoute(49, TARGET),
                    ReadRoute(49),
                    WriteEnable(49, false),
                ]);
                assert_eq!(registers.accesses, expected);
                assert_eq!(registers.is_enabled, false);
                assert_eq!(registers.route, old_route);
            }
        }
    }
}

#[def_test]
fn failed_restore_requests_mask_without_route_rollback() {
    use Access::*;
    let mut registers = Recorder::new(true);
    registers.ignores_restore = true;
    assert_eq!(
        apply(&mut registers, IrqAffinityDelivery::Preserve),
        Err(IrqAffinityApplyError::HardwareFailure)
    );
    assert_eq!(
        registers.accesses,
        [
            ReadEnable(49),
            WriteEnable(49, false),
            WaitDisable,
            ReadEnable(49),
            WriteRoute(49, TARGET),
            ReadRoute(49),
            WriteEnable(49, true),
            ReadEnable(49),
            WriteEnable(49, false),
        ]
    );
    assert_eq!(registers.is_enabled, false);
    assert_eq!(registers.route, Some(TARGET));
}

#[def_test]
fn unresolved_target_rejects_before_any_register_access() {
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        let mut registers = Recorder::new(true);
        let mut resolutions = 0;
        let mut resolved_cpu = None;
        assert_eq!(
            apply_locked(&mut registers, INTID, &candidates(), delivery, |cpu| {
                resolved_cpu = Some(cpu);
                resolutions += 1;
                None
            }),
            Err(IrqAffinityApplyError::Rejected)
        );
        assert_eq!(resolutions, 1);
        assert_eq!(resolved_cpu, Some(LogicalCpuId::new(0)));
        assert_eq!(registers.accesses.is_empty(), true);
        assert_eq!(registers.is_enabled, true);
        assert_eq!(registers.route, Some(Affinity::default()));
    }
}
