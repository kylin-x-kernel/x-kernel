// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;

use arm_gic_driver::v2::{CpuInterfaceTarget, CpuTargetMap, TargetList};
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, RawCpuId};
use kirq::{IrqAffinityApplyError, IrqAffinityDelivery};
use unittest::{assert_eq, def_test};

use super::{IntId, Registers, apply_locked, checked_intid};
use super::super::{record_cpu_target, target_for_logical_cpu};

const INTID: IntId = IntId::spi(17);
const OLD_TARGET: TargetList = TargetList::from_raw(0x40);
const NEW_TARGET: TargetList = TargetList::from_raw(0x20);

#[derive(Debug, Eq, PartialEq)]
enum Access {
    ReadEnable,
    WriteEnable(bool),
    WriteTarget(TargetList),
    ReadTarget,
}

struct Recorder {
    accesses: Vec<Access>,
    is_enabled: bool,
    target: TargetList,
    ignores_mask: bool,
    ignores_route: bool,
}

impl Recorder {
    fn new(is_enabled: bool) -> Self {
        Self {
            accesses: Vec::new(),
            is_enabled,
            target: OLD_TARGET,
            ignores_mask: false,
            ignores_route: false,
        }
    }
}

impl Registers for Recorder {
    fn is_enabled(&mut self, _: IntId) -> bool {
        self.accesses.push(Access::ReadEnable);
        self.is_enabled
    }

    fn set_enabled(&mut self, _: IntId, enabled: bool) {
        self.accesses.push(Access::WriteEnable(enabled));
        if enabled || !self.ignores_mask {
            self.is_enabled = enabled;
        }
    }

    fn set_route(&mut self, _: IntId, target: CpuInterfaceTarget) {
        let CpuInterfaceTarget::Explicit(target) = target else {
            panic!("implicit target reached the explicit register path");
        };
        self.accesses.push(Access::WriteTarget(target));
        if !self.ignores_route {
            self.target = target;
        }
    }

    fn route(&mut self, _: IntId) -> TargetList {
        self.accesses.push(Access::ReadTarget);
        self.target
    }
}

fn cpus(cpus: &[usize]) -> KCpuMask {
    let mut mask = KCpuMask::new();
    for cpu in cpus {
        mask.set_logical(LogicalCpuId::new(*cpu), true);
    }
    mask
}

fn one_cpu(cpu: usize) -> KCpuMask {
    KCpuMask::one_shot_logical(LogicalCpuId::new(cpu))
}

fn apply_explicit(
    registers: &mut Recorder,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
    target: TargetList,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    apply_locked(registers, INTID, candidates, delivery, |_| {
        Some(CpuInterfaceTarget::Explicit(target))
    })
}

#[def_test]
fn non_identity_logical_cpu_target_bit_is_written_and_read_back() {
    use Access::*;
    let routes = CpuTargetMap::new();
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(0),
            0x105,
            CpuInterfaceTarget::Explicit(NEW_TARGET),
        ),
        Ok(())
    );
    let mut registers = Recorder::new(false);
    let resolved = |cpu| {
        target_for_logical_cpu(cpu, &routes, |cpu| {
            (cpu == LogicalCpuId::new(0)).then_some(RawCpuId::new(0x105))
        })
    };
    assert_eq!(
        resolved(LogicalCpuId::new(0)),
        Some(CpuInterfaceTarget::Explicit(NEW_TARGET))
    );
    assert_eq!(resolved(LogicalCpuId::new(1)), None);
    assert_eq!(
        apply_locked(
            &mut registers,
            INTID,
            &one_cpu(0),
            IrqAffinityDelivery::Preserve,
            resolved,
        ),
        Ok(one_cpu(0))
    );
    // Logical CPU 0 selected target bit 5, not bit 0. This keeps the
    // non-identity case valid in NR_CPUS=1 builds.
    assert_eq!(registers.accesses, [WriteTarget(NEW_TARGET), ReadTarget]);
    assert_eq!(registers.target, NEW_TARGET);
}

#[def_test]
fn candidate_selection_uses_first_reachable_cpu_before_any_write() {
    use Access::*;
    // Bound the candidate set by the compile-time mask capacity so this test
    // also runs with NR_CPUS=1. On larger builds it still proves that earlier
    // unreachable candidates are skipped before the first register access.
    let candidate_count = kbuild_config::NR_CPUS.min(3);
    let candidates: Vec<usize> = (0..candidate_count).collect();
    let selected = candidate_count - 1;
    let mut registers = Recorder::new(false);
    let mut resolution_order = Vec::new();
    let result = apply_locked(
        &mut registers,
        INTID,
        &cpus(&candidates),
        IrqAffinityDelivery::Preserve,
        |cpu| {
            resolution_order.push(cpu.as_usize());
            (cpu.as_usize() == selected)
                .then_some(CpuInterfaceTarget::Explicit(NEW_TARGET))
        },
    );
    assert_eq!(result, Ok(one_cpu(selected)));
    assert_eq!(resolution_order, candidates);
    assert_eq!(registers.accesses, [WriteTarget(NEW_TARGET), ReadTarget]);
}

#[def_test]
fn all_unreachable_candidates_reject_without_mmio() {
    let candidate_count = kbuild_config::NR_CPUS.min(3);
    let candidates: Vec<usize> = (0..candidate_count).collect();
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        let mut registers = Recorder::new(true);
        let mut resolutions = 0;
        assert_eq!(
            apply_locked(
                &mut registers,
                INTID,
                &cpus(&candidates),
                delivery,
                |_| {
                    resolutions += 1;
                    None
                },
            ),
            Err(IrqAffinityApplyError::Rejected)
        );
        assert_eq!(resolutions, candidate_count);
        assert_eq!(registers.accesses.is_empty(), true);
        assert_eq!(registers.is_enabled, true);
        assert_eq!(registers.target, OLD_TARGET);
    }
}

#[def_test]
fn source_admission_rejects_private_special_foreign_and_unsupported_sources() {
    let desc = |hwirq| kirq::gic_edge_irq_desc(hwirq).with_domain(crate::gic::gic_domain());
    assert_eq!(checked_intid(desc(32), Some(64)), Some(IntId::spi(0)));
    for hwirq in [0, 15, 16, 31, 64, 1019, 1020, usize::MAX] {
        assert_eq!(checked_intid(desc(hwirq), Some(64)), None);
    }
    assert_eq!(checked_intid(desc(32), None), None);
    for rejected in [
        desc(32).with_domain(kirq::PLIC_ROOT_DOMAIN),
        desc(32).with_controller(kirq::IrqController::Plic),
        desc(32).with_flags(kirq::IrqFlags::PER_CPU),
        desc(32).with_flags(kirq::IrqFlags::MSI),
    ] {
        assert_eq!(checked_intid(rejected, Some(64)), None);
    }
}

#[def_test]
fn preserve_changes_only_the_target_and_keeps_actual_enablement() {
    use Access::*;
    for is_enabled in [false, true] {
        let mut registers = Recorder::new(is_enabled);
        assert_eq!(
            apply_explicit(
                &mut registers,
                &one_cpu(0),
                IrqAffinityDelivery::Preserve,
                NEW_TARGET,
            ),
            Ok(one_cpu(0))
        );
        assert_eq!(registers.accesses, [WriteTarget(NEW_TARGET), ReadTarget]);
        assert_eq!(registers.is_enabled, is_enabled);
        assert_eq!(registers.target, NEW_TARGET);
    }
}

#[def_test]
fn keep_masked_verifies_suppression_then_routes() {
    use Access::*;
    let mut masked = Recorder::new(false);
    assert_eq!(
        apply_explicit(
            &mut masked,
            &one_cpu(0),
            IrqAffinityDelivery::KeepMasked,
            NEW_TARGET,
        ),
        Ok(one_cpu(0))
    );
    assert_eq!(
        masked.accesses,
        [ReadEnable, WriteTarget(NEW_TARGET), ReadTarget]
    );
    assert_eq!(masked.is_enabled, false);

    let mut enabled = Recorder::new(true);
    assert_eq!(
        apply_explicit(
            &mut enabled,
            &one_cpu(0),
            IrqAffinityDelivery::KeepMasked,
            NEW_TARGET,
        ),
        Ok(one_cpu(0))
    );
    assert_eq!(
        enabled.accesses,
        [
            ReadEnable,
            WriteEnable(false),
            ReadEnable,
            WriteTarget(NEW_TARGET),
            ReadTarget
        ]
    );
    assert_eq!(enabled.is_enabled, false);
}

#[def_test]
fn failed_mask_write_never_writes_the_target() {
    use Access::*;
    let mut registers = Recorder::new(true);
    registers.ignores_mask = true;
    assert_eq!(
        apply_explicit(
            &mut registers,
            &one_cpu(0),
            IrqAffinityDelivery::KeepMasked,
            NEW_TARGET,
        ),
        Err(IrqAffinityApplyError::HardwareFailure)
    );
    assert_eq!(
        registers.accesses,
        [
            ReadEnable,
            WriteEnable(false),
            ReadEnable,
            WriteEnable(false)
        ]
    );
    // Failure requests masking; it does not claim stuck hardware obeyed.
    assert_eq!(registers.is_enabled, true);
    assert_eq!(registers.target, OLD_TARGET);
}

#[def_test]
fn route_readback_mismatch_requests_mask_without_rollback() {
    use Access::*;
    for delivery in [
        IrqAffinityDelivery::Preserve,
        IrqAffinityDelivery::KeepMasked,
    ] {
        for is_enabled in [false, true] {
            let mut registers = Recorder::new(is_enabled);
            registers.ignores_route = true;
            assert_eq!(
                apply_explicit(&mut registers, &one_cpu(0), delivery, NEW_TARGET),
                Err(IrqAffinityApplyError::HardwareFailure)
            );
            let mut expected = Vec::new();
            if delivery == IrqAffinityDelivery::KeepMasked {
                expected.push(ReadEnable);
                if is_enabled {
                    expected.push(WriteEnable(false));
                    expected.push(ReadEnable);
                }
            }
            expected.extend([WriteTarget(NEW_TARGET), ReadTarget, WriteEnable(false)]);
            assert_eq!(registers.accesses, expected);
            assert_eq!(registers.is_enabled, false);
            assert_eq!(registers.target, OLD_TARGET);
        }
    }
}

#[def_test]
fn implicit_uniprocessor_preserve_needs_no_route_access() {
    let mut registers = Recorder::new(true);
    assert_eq!(
        apply_locked(
            &mut registers,
            INTID,
            &one_cpu(0),
            IrqAffinityDelivery::Preserve,
            |_| Some(CpuInterfaceTarget::ImplicitUniprocessor),
        ),
        Ok(one_cpu(0))
    );
    assert_eq!(registers.accesses.is_empty(), true);
    assert_eq!(registers.is_enabled, true);
    assert_eq!(registers.target, OLD_TARGET);
}

#[def_test]
fn implicit_uniprocessor_keep_masked_suppresses_without_target_access() {
    use Access::*;
    for is_enabled in [false, true] {
        let mut registers = Recorder::new(is_enabled);
        assert_eq!(
            apply_locked(
                &mut registers,
                INTID,
                &one_cpu(0),
                IrqAffinityDelivery::KeepMasked,
                |_| Some(CpuInterfaceTarget::ImplicitUniprocessor),
            ),
            Ok(one_cpu(0))
        );
        let mut expected = Vec::from([ReadEnable]);
        if is_enabled {
            expected.extend([WriteEnable(false), ReadEnable]);
        }
        assert_eq!(registers.accesses, expected);
        assert_eq!(registers.is_enabled, false);
        assert_eq!(registers.target, OLD_TARGET);
    }
}

#[def_test]
fn routing_exposes_only_enable_and_target_register_operations() {
    let requested = one_cpu(0);
    let mut registers = Recorder::new(false);
    assert_eq!(
        apply_locked(
            &mut registers,
            IntId::spi(32),
            &requested,
            IrqAffinityDelivery::Preserve,
            |_| Some(CpuInterfaceTarget::Explicit(NEW_TARGET)),
        ),
        Ok(one_cpu(0))
    );
    // The narrow Registers trait has no trigger, priority, action or
    // disable-depth operation. Lifecycle and disable-depth invariants belong
    // to the existing KIRQ affinity tests, not this register fixture.
    assert_eq!(requested, one_cpu(0));
    assert_eq!(
        registers.accesses,
        [Access::WriteTarget(NEW_TARGET), Access::ReadTarget]
    );
}
