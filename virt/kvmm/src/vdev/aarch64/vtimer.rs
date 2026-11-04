// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-backed guest virtual timer delivery.

use aarch64_cpu::registers::{CNTFRQ_EL0, CNTPCT_EL0, CNTVOFF_EL2, Readable};

use super::irq_route::HOST_VTIMER_IRQ;
#[path = "timer_residency.rs"]
mod residency;
pub(crate) use residency::TimerResidency;

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

    fn on_entry_failure(&mut self, _vcpu_id: u32) {
        // SAFETY: this hook executes at EL2 with IRQs masked on the CPU whose
        // device context was installed. Stop its timer before detaching wakeup.
        unsafe {
            core::arch::asm!("msr S3_5_C14_C3_1, xzr", "isb", options(nostack));
        }
        super::irq_route::clear_owner_for_current_task();
    }
}

/// Hook factory for the host-backed virtual timer.
pub struct VtimerHookFactory;

impl VtimerHookFactory {
    /// Prepare the permanently owned host route before publishing any vCPU.
    /// Must run in sleepable task context; unsupported controllers fail closed.
    pub fn prepare_route() -> bool {
        super::irq_route::prepare_host_vtimer_route()
    }
}

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
    if is_timer_asserted(vcpu) {
        vcpu.vm.inject_irq(vcpu.vcpu_id, HOST_VTIMER_IRQ);
        true
    } else {
        false
    }
}

pub(crate) fn is_timer_asserted(vcpu: &Vcpu<Aarch64Vhe>) -> bool {
    remaining_ticks(
        vcpu.arch.cntv_ctl,
        vcpu.arch.cntv_cval,
        CNTVOFF_EL2.get(),
        CNTPCT_EL0.get(),
    ) == Some(0)
}

fn remaining_ticks(
    control: u64,
    compare: u64,
    offset_ticks: u64,
    physical_now: u64,
) -> Option<u64> {
    // Like KVM's kvm_counter_compute_delta, compare in the guest counter
    // domain. Casting the difference to i64 misclassifies CVAL=u64::MAX.
    (control & 3 == 1).then(|| compare.saturating_sub(physical_now.wrapping_sub(offset_ticks)))
}

/// Translate this vCPU's remaining virtual-counter interval into a host wait.
/// Called with IRQs masked, so the counter offset belongs to this resident.
pub(crate) fn blocking_deadline(vcpu: &Vcpu<Aarch64Vhe>) -> Option<ktime_types::MonotonicInstant> {
    let host_now = khal::time::monotonic_time();
    let ticks = remaining_ticks(
        vcpu.arch.cntv_ctl,
        vcpu.arch.cntv_cval,
        CNTVOFF_EL2.get(),
        CNTPCT_EL0.get(),
    )?;
    if ticks == 0 {
        return Some(host_now);
    }
    let nanos = ((ticks as u128 * 1_000_000_000).div_ceil(CNTFRQ_EL0.get() as u128))
        .min(u64::MAX as u128) as u64;
    Some(host_now + ktime_types::TimeSpan::from_nanos(nanos))
}
