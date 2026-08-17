// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal RISC-V virtual timer injection.
//!
//! This backend injects VS timer interrupts from SBI-programmed deadlines.

use crate::{
    arch::riscv64::{self, RiscvHext},
    vcpu::Vcpu,
    vdev::{VcpuHook, VcpuHookFactory},
};

/// Hook factory for the minimal RISC-V virtual timer.
pub struct RiscvTimerHookFactory;

impl VcpuHookFactory<RiscvHext> for RiscvTimerHookFactory {
    fn make_vcpu_hook(&self, _vcpu_id: u32) -> Option<alloc::boxed::Box<dyn VcpuHook<RiscvHext>>> {
        Some(alloc::boxed::Box::new(RiscvTimerHook))
    }
}

struct RiscvTimerHook;

impl VcpuHook<RiscvHext> for RiscvTimerHook {
    fn on_entry(&mut self, vcpu: &mut Vcpu<RiscvHext>) {
        let now = riscv64::read_time();
        let deadline = vcpu.arch.timer_deadline;
        riscv64::set_virtual_timer_irq_pending(deadline != 0 && now >= deadline);
    }

    fn on_exit(&mut self, _vcpu_id: u32) {
        riscv64::set_virtual_timer_irq_pending(false);
    }
}
