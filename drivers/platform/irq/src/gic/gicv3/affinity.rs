// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immediate routing for this singleton GICv3's ordinary standard SPIs.
//!
//! Line exclusion precedes GIC exclusion. The latter spans delivery sampling,
//! disable completion, route readback and optional enable restoration. Routing
//! never changes trigger/priority or completes an old claim on another CPU.

use arm_gic_driver::{
    IntId,
    v3::{Affinity, Gic},
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

fn apply(
    desc: IrqDesc,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    if super::super::ACTIVE_GIC.get() != Some(&super::super::GicVersion::V3) {
        return Err(IrqAffinityApplyError::Rejected);
    }
    let intid = checked_intid(desc, super::super::STANDARD_INTID_LIMIT.get().copied())
        .ok_or(IrqAffinityApplyError::Rejected)?;
    let gic = super::GIC.get().ok_or(IrqAffinityApplyError::Rejected)?;
    // The callback contract supplies interrupt-ready candidates. On this
    // singleton AArch64 backend, BSP/AP readiness is published only after
    // gic::init_current_cpu succeeds: its CPU interface locates and wakes this
    // GIC's matching redistributor. This is not a generic implication from
    // readiness to reachability on arbitrary controllers. CPU maps and ready
    // publications are boot-monotonic; CPU hot-unplug is not supported here.
    apply_locked(&mut *gic.lock(), intid, candidates, delivery, |cpu| {
        raw_cpu_id(cpu).map(|raw| Affinity::from_mpidr(raw.as_usize() as u64))
    })
}

/// Register access under one GIC guard; injected tests need no real MMIO.
trait Registers {
    fn is_enabled(&mut self, intid: IntId) -> bool;
    fn set_enabled(&mut self, intid: IntId, enabled: bool);
    fn wait_for_disable(&mut self) -> bool;
    fn set_route(&mut self, intid: IntId, target: Affinity);
    fn route(&mut self, intid: IntId) -> Option<Affinity>;
}

impl Registers for Gic {
    fn is_enabled(&mut self, intid: IntId) -> bool {
        self.is_irq_enable(intid)
    }

    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        self.set_irq_enable(intid, enabled);
    }

    fn wait_for_disable(&mut self) -> bool {
        self.wait_for_distributor_rwp().is_ok()
    }

    fn set_route(&mut self, intid: IntId, target: Affinity) {
        self.set_target_cpu(intid, Some(target));
    }

    fn route(&mut self, intid: IntId) -> Option<Affinity> {
        self.get_target_cpu(intid)
    }
}

/// `intid` is a validated standard SPI; `resolve` returns reachable candidates.
/// Production retains the GIC guard across this whole bounded transaction.
fn apply_locked(
    registers: &mut impl Registers,
    intid: IntId,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
    mut resolve: impl FnMut(LogicalCpuId) -> Option<Affinity>,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    let (cpu, target) = candidates
        .iter_logical()
        .find_map(|cpu| resolve(cpu).map(|target| (cpu, target)))
        .ok_or(IrqAffinityApplyError::Rejected)?;
    // Linux gic_set_affinity also samples actual hardware enablement, not the
    // logical descriptor flag, before masking and writing IROUTER.
    let was_enabled = registers.is_enabled(intid);
    if was_enabled {
        registers.set_enabled(intid, false);
    }
    // Even an already-clear enable bit can have an outstanding disable. RWP
    // completes that disable, not IROUTER programming or active IRQ execution.
    if registers.wait_for_disable() && !registers.is_enabled(intid) {
        registers.set_route(intid, target);
        if registers.route(intid) == Some(target) {
            if delivery == IrqAffinityDelivery::KeepMasked || !was_enabled {
                return Ok(KCpuMask::one_shot_logical(cpu));
            }
            registers.set_enabled(intid, true);
            if registers.is_enabled(intid) {
                return Ok(KCpuMask::one_shot_logical(cpu));
            }
        }
    }
    // A failed readback or disable wait cannot certify the old or new route.
    // Request suppression without restoring delivery or promising quiescence.
    registers.set_enabled(intid, false);
    Err(IrqAffinityApplyError::HardwareFailure)
}

unittest::path_mod_test!(tests, "../../../ut-tests/gicv3_affinity.rs");
