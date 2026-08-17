// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-backed guest virtual timer delivery.

use super::irq_route::HOST_VTIMER_IRQ;
use crate::{
    arch::aarch64::Aarch64Vhe,
    vcpu::Vcpu,
    vdev::{VcpuHook, VcpuHookFactory},
};

/// Host virtual-timer hook run in the vCPU world-switch entry window.
pub struct HostVtimerHook;

impl VcpuHook<Aarch64Vhe> for HostVtimerHook {
    fn on_entry(&mut self, vcpu: &mut Vcpu<Aarch64Vhe>) {
        super::irq_route::publish_owner_for_current_cpu();
        check_vtimer(vcpu);
    }

    fn on_exit(&mut self, _vcpu_id: u32) {}
}

/// Hook factory for the host-backed virtual timer.
pub struct VtimerHookFactory;

impl VcpuHookFactory<Aarch64Vhe> for VtimerHookFactory {
    fn make_vcpu_hook(&self, _vcpu_id: u32) -> Option<alloc::boxed::Box<dyn VcpuHook<Aarch64Vhe>>> {
        Some(alloc::boxed::Box::new(HostVtimerHook))
    }
}

pub(crate) fn set_host_vtimer_irq_enabled(enabled: bool) {
    super::irq_route::set_host_vtimer_irq_enabled(enabled);
}

pub(crate) fn clear_host_vtimer_owner_for_current_task() {
    super::irq_route::clear_owner_for_current_task();
}

/// Deliver the guest virtual timer tick if it expired while the guest ran.
///
/// The world-switch exit stub saved `CNTV_CTL`/`CNTV_CVAL` into the vCPU. Here
/// we recompute expiry against the physical counter (`deadline = CVAL +
/// CNTVOFF`) and queue vPPI 27 if due.
fn check_vtimer(vcpu: &mut Vcpu<Aarch64Vhe>) -> bool {
    const CTL_ENABLE: u64 = 1 << 0;
    const CTL_IMASK: u64 = 1 << 1;
    let ctl = vcpu.arch.cntv_ctl;
    if ctl & CTL_ENABLE == 0 || ctl & CTL_IMASK != 0 {
        return false;
    }
    let now: u64;
    let cntvoff: u64;
    // SAFETY: reading CNTPCT_EL0 / CNTVOFF_EL2 from EL2 is always safe.
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) now);
        core::arch::asm!("mrs {}, cntvoff_el2", out(reg) cntvoff);
    }
    if now >= vcpu.arch.cntv_cval.wrapping_add(cntvoff) {
        vcpu.vm.inject_irq(vcpu.vcpu_id, HOST_VTIMER_IRQ);
        true
    } else {
        false
    }
}
