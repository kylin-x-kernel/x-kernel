// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-Line local control slots. All access holds the Line's IRQ-saving lock;
//! these are not remotely writable per-CPU globals or an online CPU inventory.

use super::{IrqDesc, IrqPlatformPlan};

/// Retains the matching controller teardown obligation even when preparation
/// fails. Readiness permits enable; absence alone means teardown is complete.
#[derive(Clone, Copy)]
struct NmiPreparation {
    chip: &'static crate::PerCpuNmiChip,
    is_ready: bool,
}

/// One CPU's software control state within a PerCpu Line.
/// Hardware plans commit here under the Line lock; this record does not prove
/// that the external producer has stopped or that its final claim has returned.
#[derive(Clone, Copy)]
pub(super) struct LocalControl {
    pub(super) is_enabled: bool,
    pub(super) has_failed_configuration: bool,
    configured_generation: Option<u64>,
    nmi: Option<NmiPreparation>,
}

impl LocalControl {
    pub(super) const fn new() -> Self {
        Self {
            is_enabled: false,
            has_failed_configuration: false,
            configured_generation: None,
            nmi: None,
        }
    }

    pub(super) fn has_nmi_preparation(&self) -> bool {
        self.nmi.is_some()
    }

    pub(super) fn has_failed_nmi_preparation(&self) -> bool {
        self.nmi.is_some_and(|nmi| !nmi.is_ready)
    }

    pub(super) fn prepare_nmi(
        &mut self,
        desc: IrqDesc,
        chip: &'static crate::PerCpuNmiChip,
    ) -> Result<(), crate::IrqDescError> {
        if let Some(nmi) = self.nmi {
            if !core::ptr::eq(nmi.chip, chip) {
                return Err(crate::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq });
            }
            if nmi.is_ready {
                return Ok(());
            }
        }
        if self.is_enabled {
            return Err(crate::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq });
        }
        // A failing backend may have changed attributes. Retain its exact
        // cleanup operations before the first write, and keep enable closed.
        self.nmi = Some(NmiPreparation {
            chip,
            is_ready: false,
        });
        (chip.prepare)(desc)?;
        self.nmi = Some(NmiPreparation {
            chip,
            is_ready: true,
        });
        Ok(())
    }

    pub(super) fn teardown_nmi(&mut self, desc: IrqDesc) -> Result<(), crate::IrqDescError> {
        if self.is_enabled {
            return Err(crate::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq });
        }
        if let Some(nmi) = self.nmi {
            self.nmi = Some(NmiPreparation {
                is_ready: false,
                ..nmi
            });
            (nmi.chip.teardown)(desc)?;
            self.nmi = None;
        }
        Ok(())
    }

    fn plan(&self, desc: IrqDesc, cpu: usize) -> IrqPlatformPlan {
        IrqPlatformPlan {
            cpu: Some(cpu),
            // Disable nesting belongs to ordinary shared Lines, not local gates.
            ..IrqPlatformPlan::none(desc, 0)
        }
    }

    pub(super) fn enable(&self, desc: IrqDesc, generation: u64, cpu: usize) -> IrqPlatformPlan {
        let mut plan = self.plan(desc, cpu);
        plan.configure =
            self.has_failed_configuration || self.configured_generation != Some(generation);
        plan.enable = Some(true);
        plan
    }

    pub(super) fn register_disabled(&self, desc: IrqDesc, cpu: usize) -> IrqPlatformPlan {
        IrqPlatformPlan {
            // Mask an inherited enabled source before publishing disabled state.
            enable: self.is_enabled.then_some(false),
            ..self.plan(desc, cpu)
        }
    }

    pub(super) fn reconfigure(
        &self,
        desc: IrqDesc,
        generation: u64,
        cpu: usize,
    ) -> IrqPlatformPlan {
        IrqPlatformPlan {
            configure: self.has_failed_configuration
                || self.configured_generation != Some(generation),
            ..self.plan(desc, cpu)
        }
    }

    pub(super) fn disable(&self, desc: IrqDesc, cpu: usize) -> IrqPlatformPlan {
        IrqPlatformPlan {
            // Like Linux irq_percpu_disable, each call closes the local gate;
            // repeated shutdown never accumulates credits for a future enable.
            enable: Some(false),
            ..self.plan(desc, cpu)
        }
    }

    pub(super) fn commit(&mut self, plan: IrqPlatformPlan, generation: u64) {
        if plan.configure {
            self.configured_generation = Some(generation);
            self.has_failed_configuration = false;
        }
        if let Some(enabled) = plan.enable {
            self.is_enabled = enabled;
        }
    }

    pub(super) fn can_run(&self) -> bool {
        self.is_enabled && !self.has_failed_configuration
    }
}

pub(super) fn current_cpu() -> usize {
    // The caller's Line guard and outer handoff guard retain IRQ/preemption
    // exclusion through sampling, hardware apply and commit. No nested guard.
    debug_assert!(!karch::local_irq_enabled());
    kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize()
}

unittest::path_mod_test!(tests_local_control, "../../ut-tests/local_control.rs");
