// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv2 backend.

use arm_gic_driver::v2::*;
use kirq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
const GUEST_VTIMER_IRQ: usize = 27;

pub fn init(gicd_base: memaddr::VirtAddr, gicc_base: memaddr::VirtAddr) {
    if GIC.get().is_some() {
        return;
    }
    info!("Initialize GICv2...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicc_base = VirtAddr::new(gicc_base.into());
    // SAFETY: both distributor and CPU interface bases come from platform
    // discovery and point at the mapped GICv2 MMIO frames for this machine.
    let mut gic = unsafe { Gic::new(gicd_base, gicc_base, None) };
    gic.init();
    #[cfg(feature = "pmr")]
    karch::pmr::init(usize::from(gic.gicc_addr()));
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}

pub fn init_current_cpu() {
    debug!("Initialize GICv2 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    cpu.init_current_cpu();
    cpu.set_eoi_mode_ns(true);
}

pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv2 set trigger: {interrupt_id} {edge}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv2 set enable: {irq} {enabled}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
}

#[cfg(feature = "pmr")]
pub fn set_prio(irq: usize, priority: u8) {
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

#[cfg(not(feature = "pmr"))]
pub fn set_prio(_irq: usize, _priority: u8) {
    unreachable!()
}

/// IRQ path: ack + early EOI.  GICv2 has no pseudo‑NMI support.
/// Returns (hwirq, cookie, false).  Mirrors `__gic_handle_irq_from_irqson`.
pub fn dispatch_irq_from_irqson() -> Option<(usize, usize, bool)> {
    let ack = TRAP_OP.ack();
    if ack.is_special() {
        return None;
    }

    let irq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;
    trace!("IRQ: {ack:?}");

    // GIC non-secure mode requires an early EOI to lower the running priority,
    // while the actual deactivation is deferred to `complete_irq`.
    TRAP_OP.eoi(ack);
    // Order the acknowledged interrupt before the handler.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    Some((irq, u32::from(ack) as usize, false))
}

/// NMI path: lower PMR to NMI_ONLY before ack, then restore.
/// Mirrors `__gic_handle_irq_from_irqsoff`.
pub fn dispatch_irq_from_irqsoff() -> Option<(usize, usize)> {
    #[cfg(feature = "nmi-pmu")]
    let saved_pmr = karch::pmr::read();
    #[cfg(feature = "nmi-pmu")]
    {
        karch::pmr::write(karch::pmr::NMI_ONLY);
        // Ensure the PMR write is visible before the IAR read.
        aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    }

    let result = dispatch_irq_from_irqson();

    // Restore PMR even on a spurious/EOI-only ack so the NMI window does
    // not leak into the interrupted context.
    #[cfg(feature = "nmi-pmu")]
    karch::pmr::write(saved_pmr);

    let (irq, cookie, _) = result?;
    Some((irq, cookie))
}

fn skip_dir_for_vmm_guest_irq(ack: Ack) -> bool {
    if !kbuild_config::KFEAT_VMM {
        return false;
    }
    let hwirq = match ack {
        Ack::Other(intid) => intid,
        Ack::SGI { intid, cpu_id: _ } => intid,
    }
    .to_u32() as usize;

    // KVM-style GICv2 HW LR injection for the guest virtual timer keeps the
    // physical PPI active until the guest EOI/deactivate path consumes the HW
    // LR. Host-side DIR here would drop the physical backing interrupt before
    // the guest observes it.
    hwirq == GUEST_VTIMER_IRQ
}

pub fn complete_irq(completion_cookie: usize) {
    let ack = Ack::from(completion_cookie as u32);
    if skip_dir_for_vmm_guest_irq(ack) {
        return;
    }
    TRAP_OP.dir(ack);
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    // `dmb ishst` orders the caller's prior Normal-memory stores before the
    // SGI MMIO write, which is required for cross-CPU publish-before-notify
    // semantics.
    aarch64_cpu::asm::barrier::dmb(aarch64_cpu::asm::barrier::ISHST);
    match target {
        TargetCpu::Self_ => {
            GIC.lock()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::Current);
        }
        TargetCpu::Specific(cpu_id) => {
            let target_list = TargetList::new(core::iter::once(cpu_id));
            GIC.lock().send_sgi(
                IntId::sgi(interrupt_id as u32),
                SGITarget::TargetList(target_list),
            );
        }
        TargetCpu::AllButSelf { .. } => {
            GIC.lock()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::AllOther);
        }
    }
}
