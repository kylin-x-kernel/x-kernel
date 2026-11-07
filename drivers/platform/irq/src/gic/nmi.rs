// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local PPI attribute transactions, separate from PMU producer control.

use super::{GIC, Gic, IntId};

pub(crate) static CHIP: kirq::PerCpuNmiChip = kirq::PerCpuNmiChip {
    prepare: |desc| update(desc, true),
    teardown: |desc| update(desc, false),
};

fn update(desc: kirq::IrqDesc, is_nmi: bool) -> Result<(), kirq::IrqDescError> {
    if !super::super::is_gic_domain(desc.domain) || !(16..32).contains(&desc.hwirq) {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    let gic = GIC
        .get()
        .ok_or(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })?;
    // SAFETY: the checked descriptor is a standard PPI with no truncation.
    let intid = unsafe { IntId::raw(desc.hwirq as u32) };
    update_locked(&mut *gic.lock(), intid, is_nmi)
}

trait Registers {
    fn wait_for_disable(&mut self) -> bool;
    fn is_enabled(&self, intid: IntId) -> bool;
    fn is_active(&self, intid: IntId) -> bool;
    fn clear_pending(&mut self, intid: IntId);
    fn is_pending(&self, intid: IntId) -> bool;
    fn set_attributes(&mut self, intid: IntId, is_nmi: bool) -> bool;
}

impl Registers for Gic {
    fn wait_for_disable(&mut self) -> bool {
        self.cpu_interface().wait_for_redistributor_rwp().is_ok()
    }

    fn is_enabled(&self, intid: IntId) -> bool {
        self.is_irq_enable(intid)
    }

    fn is_active(&self, intid: IntId) -> bool {
        Gic::is_active(self, intid)
    }

    fn clear_pending(&mut self, intid: IntId) {
        self.set_pending(intid, false);
    }

    fn is_pending(&self, intid: IntId) -> bool {
        Gic::is_pending(self, intid)
    }

    fn set_attributes(&mut self, intid: IntId, is_nmi: bool) -> bool {
        #[cfg(feature = "nmi-hardware")]
        {
            let attribute = if is_nmi {
                super::NmiAttribute::NonMaskable
            } else {
                super::NmiAttribute::Maskable
            };
            self.set_nmi_attribute(intid, attribute).is_ok()
                && self.nmi_attribute(intid) == Ok(attribute)
        }
        #[cfg(not(feature = "nmi-hardware"))]
        {
            let priority = if is_nmi { 0 } else { karch::pmr::IRQ_THRESHOLD };
            self.set_priority(intid, priority);
            self.get_priority(intid) == priority
        }
    }
}

fn update_locked(
    registers: &mut impl Registers,
    intid: IntId,
    is_nmi: bool,
) -> Result<(), kirq::IrqDescError> {
    let failure = kirq::IrqDescError::ConfigurationFailed {
        hwirq: intid.to_u32() as usize,
    };
    // A completed disable write is distinct from an inactive interrupt. Do not
    // clear active state: only the original claim owner may complete that claim.
    if !registers.wait_for_disable() || registers.is_enabled(intid) || registers.is_active(intid) {
        return Err(failure);
    }
    if !is_nmi {
        // Teardown's caller has stopped the producer. Pending state must not
        // survive attribute demotion and later reuse by a different handler.
        registers.clear_pending(intid);
        if registers.is_pending(intid) {
            return Err(failure);
        }
    }
    if !registers.set_attributes(intid, is_nmi) {
        return Err(failure);
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/gicv3_nmi.rs");
