// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordinary, non-forwarded standard SPI operations shared by GICv2 and GICv3.

use arm_gic_driver::IntId;

pub(super) static CHIP: kirq::FasteoiChip = kirq::FasteoiChip {
    mask: |desc| set_delivery(desc, false),
    unmask: |desc| set_delivery(desc, true),
    resend: kirq::FasteoiResend::HardwareOnly(retrigger),
};

fn is_spi_binding(desc: kirq::IrqDesc) -> bool {
    matches!(
        super::select_gic_flow(desc, super::STANDARD_INTID_LIMIT.get().copied()),
        Ok(kirq::IrqFlow::Fasteoi(_))
    )
}

fn set_delivery(desc: kirq::IrqDesc, enabled: bool) {
    // The capability's function pointers are publicly callable. Revalidate at
    // that boundary rather than letting an unrelated descriptor reach MMIO.
    if is_spi_binding(desc) {
        super::enable(desc.hwirq, enabled);
    }
}

fn retrigger(desc: kirq::IrqDesc) -> bool {
    if !is_spi_binding(desc) {
        return false;
    }
    // Flow selection and the retained Line validate this immutable binding.
    // There is no software fallback: ISPENDR requests a new hardware claim,
    // whose original cookie still owns its own EOI/deactivation obligation.
    match super::active_version() {
        super::GicVersion::V2 => super::gicv2::retrigger_spi(desc.hwirq),
        super::GicVersion::V3 => super::gicv3::retrigger_spi(desc.hwirq),
    }
    true
}

/// Backend implementations hold their GIC lock across each helper call.
pub(super) trait Registers {
    fn set_enabled(&mut self, intid: IntId, enabled: bool);
    fn wait_for_disable(&mut self) -> bool;
    fn set_pending(&mut self, intid: IntId);
}

/// Returns whether the disable completion wait succeeded, not hardware drain.
pub(super) fn set_enabled(registers: &mut impl Registers, intid: IntId, enabled: bool) -> bool {
    registers.set_enabled(intid, enabled);
    // Linux gic_mask_irq waits for Distributor RWP; unmask needs no such wait.
    // GICv2 has no RWP register and supplies an immediate successful result.
    enabled || registers.wait_for_disable()
}

pub(super) fn set_pending(registers: &mut impl Registers, intid: IntId) {
    registers.set_pending(intid);
    // Do not read pending back: another CPU can acknowledge and clear it before
    // readback. RWP does not track ISPENDR. Issuing this W1S is acceptance, not
    // evidence of delivery, handler completion or a recoverable timeout.
}

unittest::path_mod_test!(tests, "../../ut-tests/gic_spi.rs");
