// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immediate routing for this singleton GICv2's ordinary standard SPIs.
//!
//! Line exclusion precedes GIC exclusion. Linux `gic_set_affinity` writes the
//! discovered `gic_cpu_map` byte to `GICD_ITARGETSR` without masking or waiting
//! for a distributor RWP, because that byte register has no GICv3-style pending
//! write protocol. This backend follows that protocol: `Preserve` changes only
//! the target byte and verifies its readback. It does not touch trigger,
//! priority, action ownership or the completion path. The existing SGI/IPI
//! target-list transport is separate; this map is not a second CPU-online
//! state and does not change notify delivery.

use arm_gic_driver::{
    IntId,
    v2::{CpuInterfaceTarget, CpuTargetMap, Gic, TargetList},
};
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, raw_cpu_id};
use kirq::{IrqAffinityApplyError, IrqAffinityDelivery, IrqDesc};

pub(in crate::gic) static CHIP: kirq::IrqAffinityChip = kirq::IrqAffinityChip { apply };

fn checked_intid(desc: IrqDesc, limit: Option<usize>) -> Option<IntId> {
    if !(32..1020).contains(&desc.hwirq)
        || !matches!(
            super::super::select_gic_flow(desc, limit),
            Ok(kirq::IrqFlow::Fasteoi(_))
        )
    {
        return None;
    }
    // The constructor takes an SPI-relative number. The validated standard
    // range above excludes private/special IDs before subtraction or narrowing.
    Some(IntId::spi((desc.hwirq - 32) as u32))
}

fn resolve_target(cpu: LogicalCpuId, targets: &CpuTargetMap) -> Option<CpuInterfaceTarget> {
    super::target_for_logical_cpu(cpu, targets, raw_cpu_id)
}

fn apply(
    desc: IrqDesc,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    if super::super::ACTIVE_GIC.get() != Some(&super::super::GicVersion::V2) {
        return Err(IrqAffinityApplyError::Rejected);
    }
    let intid = checked_intid(desc, super::super::STANDARD_INTID_LIMIT.get().copied())
        .ok_or(IrqAffinityApplyError::Rejected)?;
    let gic = super::GIC.get().ok_or(IrqAffinityApplyError::Rejected)?;
    // Candidates are interrupt-ready logical CPUs. The boot map attaches a
    // distinct, discovered CPU-interface bit to each before readiness is
    // published; a logical CPU number is never used as that bit.
    apply_locked(&mut *gic.lock(), intid, candidates, delivery, |cpu| {
        resolve_target(cpu, &super::CPU_TARGETS)
    })
}

/// Register access under one GIC guard; injected tests need no real MMIO.
trait Registers {
    fn is_enabled(&mut self, intid: IntId) -> bool;
    fn set_enabled(&mut self, intid: IntId, enabled: bool);
    fn set_route(&mut self, intid: IntId, target: CpuInterfaceTarget);
    fn route(&mut self, intid: IntId) -> TargetList;
}

impl Registers for Gic {
    fn is_enabled(&mut self, intid: IntId) -> bool {
        self.is_irq_enable(intid)
    }

    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        self.set_irq_enable(intid, enabled);
    }

    fn set_route(&mut self, intid: IntId, target: CpuInterfaceTarget) {
        self.route_interrupt_to_cpu(intid, target);
    }

    fn route(&mut self, intid: IntId) -> TargetList {
        self.get_target_cpu(intid)
    }
}

/// Establishes and verifies suppression without claiming hardware quiescence.
fn suppress(registers: &mut impl Registers, intid: IntId) -> Result<(), IrqAffinityApplyError> {
    if registers.is_enabled(intid) {
        registers.set_enabled(intid, false);
        if registers.is_enabled(intid) {
            // Request masking again so a caller cannot mistake this failure for
            // an already-masked source, but do not report quiescence.
            registers.set_enabled(intid, false);
            return Err(IrqAffinityApplyError::HardwareFailure);
        }
    }
    Ok(())
}

/// `intid` is a validated standard SPI; `resolve` returns reachable targets.
/// Production retains the GIC guard across this whole bounded transaction.
fn apply_locked(
    registers: &mut impl Registers,
    intid: IntId,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
    mut resolve: impl FnMut(LogicalCpuId) -> Option<CpuInterfaceTarget>,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    let (cpu, target) = candidates
        .iter_logical()
        .find_map(|cpu| resolve(cpu).map(|target| (cpu, target)))
        .ok_or(IrqAffinityApplyError::Rejected)?;
    // Rejection above performed no register access, so an unreachable target
    // set has zero MMIO side effects.
    let CpuInterfaceTarget::Explicit(target) = target else {
        // A uniprocessor controller routes every SPI to its sole interface
        // implicitly and exposes ITARGETSR as RAZ/WI. Preserve therefore needs
        // no route transaction or impossible readback. KeepMasked still has to
        // establish suppression through the implemented enable register.
        if delivery == IrqAffinityDelivery::KeepMasked {
            suppress(registers, intid)?;
        }
        return Ok(KCpuMask::one_shot_logical(cpu));
    };

    if delivery == IrqAffinityDelivery::KeepMasked {
        suppress(registers, intid)?;
    }
    registers.set_route(intid, CpuInterfaceTarget::Explicit(target));
    if registers.route(intid) == target {
        return Ok(KCpuMask::one_shot_logical(cpu));
    }
    // A failed readback cannot certify the old or new destination. Request
    // masking without restoring delivery or promising quiescence.
    registers.set_enabled(intid, false);
    Err(IrqAffinityApplyError::HardwareFailure)
}

unittest::path_mod_test!(tests, "../../../ut-tests/gicv2_affinity.rs");
