// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-Line local control slots. All access holds the Line's IRQ-saving lock;
//! these are not remotely writable per-CPU globals or an online CPU inventory.

use super::{IrqDesc, IrqPlatformPlan};

#[derive(Clone, Copy)]
pub(super) struct LocalControl {
    pub(super) is_enabled: bool,
    pub(super) has_failed_configuration: bool,
    configured_generation: Option<u64>,
    disable_depth: usize,
}

impl LocalControl {
    pub(super) const fn new() -> Self {
        Self {
            is_enabled: false,
            has_failed_configuration: false,
            configured_generation: None,
            disable_depth: 0,
        }
    }

    fn plan(&self, desc: IrqDesc, cpu: usize) -> IrqPlatformPlan {
        IrqPlatformPlan {
            cpu: Some(cpu),
            ..IrqPlatformPlan::none(desc, self.disable_depth)
        }
    }

    pub(super) fn enable(
        &self,
        desc: IrqDesc,
        generation: u64,
        cpu: usize,
        force: bool,
        auto: bool,
    ) -> IrqPlatformPlan {
        let mut plan = self.plan(desc, cpu);
        plan.disable_depth = if auto {
            0
        } else {
            self.disable_depth.saturating_sub(1)
        };
        if plan.disable_depth == 0 {
            plan.configure =
                self.has_failed_configuration || self.configured_generation != Some(generation);
            plan.enable =
                (force || !self.is_enabled || self.has_failed_configuration).then_some(true);
        }
        plan
    }

    pub(super) fn register_disabled(&self, desc: IrqDesc, cpu: usize) -> IrqPlatformPlan {
        IrqPlatformPlan {
            // Legacy enable can precede action installation. Mask before
            // committing a nonzero depth, or nested disable would skip it.
            enable: self.is_enabled.then_some(false),
            disable_depth: self.disable_depth.max(1),
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
            enable: (self.disable_depth == 0 && self.is_enabled).then_some(false),
            disable_depth: self.disable_depth.saturating_add(1),
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
        self.disable_depth = plan.disable_depth;
    }

    pub(super) fn can_run(&self) -> bool {
        self.is_enabled && self.disable_depth == 0 && !self.has_failed_configuration
    }
}

pub(super) fn current_cpu() -> usize {
    // The caller's Line guard and outer handoff guard retain IRQ/preemption
    // exclusion through sampling, hardware apply and commit. No nested guard.
    debug_assert!(!karch::local_irq_enabled());
    kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize()
}

unittest::path_mod_test!(tests_local_control, "../../ut-tests/local_control.rs");
