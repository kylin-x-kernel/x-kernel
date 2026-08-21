// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv3 backend.

use arm_gic_driver::v3::*;
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kirq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();

pub fn init(gicd_base: memaddr::VirtAddr, gicr_base: memaddr::VirtAddr) {
    if GIC.get().is_some() {
        return;
    }
    info!("Initialize GICv3...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicr_base = VirtAddr::new(gicr_base.into());
    // SAFETY: both distributor and redistributor bases come from platform
    // discovery and point at the mapped GICv3 MMIO frames for this machine.
    let mut gic = unsafe { Gic::new(gicd_base, gicr_base) };
    gic.init();
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}

pub fn init_current_cpu() {
    debug!("Initialize GICv3 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    let _ = cpu.init_current_cpu();
    cpu.set_eoi_mode(true);
}

pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv3 set trigger: {interrupt_id} {edge}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv3 set enable: {irq} {enabled}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let mut gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
}

pub fn set_prio(irq: usize, priority: u8) {
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

/// IRQ path: ack1 → check RPR → early EOI1.
/// Returns (hwirq, cookie, is_nmi).  Caller decides window based on is_nmi.
/// Mirrors `__gic_handle_irq_from_irqson`.
pub fn dispatch_irq_from_irqson() -> Option<(usize, usize, bool)> {
    let ack = TRAP_OP.ack1();
    if ack.is_special() {
        return None;
    }
    let irq = ack.to_u32() as usize;

    // After ack, running priority reflects this interrupt's priority.  NMI
    // sources are programmed at priority 0.  The RPR read only decides
    // whether to open the NMI window, which exists solely under `nmi-pmu`;
    // skip it otherwise to keep the IRQ hot path free of system-register
    // traffic.
    #[cfg(feature = "nmi-pmu")]
    let is_nmi = {
        let rpr: u64;
        // SAFETY: reading ICC_RPR_EL1, a read-only system register.
        unsafe { core::arch::asm!("mrs {}, ICC_RPR_EL1", out(reg) rpr, options(nomem, nostack)) };
        rpr == 0
    };
    #[cfg(not(feature = "nmi-pmu"))]
    let is_nmi = false;

    TRAP_OP.eoi1(ack);
    // Order the acknowledged interrupt before the handler.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    Some((irq, irq, is_nmi))
}

/// NMI path: lower PMR to NMI_ONLY before ack1, then restore.
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

    let ack = TRAP_OP.ack1();
    let result = if ack.is_special() {
        None
    } else {
        // Ack without re-reading ICC_RPR_EL1: the caller already knows this
        // is an NMI, and the read would otherwise be duplicated on every
        // pseudo-NMI.
        let irq = ack.to_u32() as usize;
        TRAP_OP.eoi1(ack);
        // Order the acknowledged interrupt before the handler.
        aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
        Some((irq, irq))
    };

    #[cfg(feature = "nmi-pmu")]
    karch::pmr::write(saved_pmr);

    result
}

pub fn complete_irq(completion_cookie: usize) {
    // SAFETY: completion_cookie is the INTID returned by ack1() in
    // dispatch_irq on the same CPU; it is always a valid GICv3
    // interrupt identifier (special values already filtered out).
    let intid = unsafe { IntId::raw(completion_cookie as u32) };
    TRAP_OP.dir(intid);
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    // `ICC_SGI1R_EL1` is written via a system-register `msr`, so we need
    // `dsb st` here rather than a mere ordering barrier. This guarantees the
    // shootdown request state stored in Normal memory has completed before the
    // target CPU can observe the SGI delivery.
    aarch64_cpu::asm::barrier::dsb(aarch64_cpu::asm::barrier::ST);
    match target {
        TargetCpu::Self_ => {
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::current());
        }
        TargetCpu::Specific(logical_cpu_id) => {
            let Some(raw_cpu_id) = raw_cpu_id(LogicalCpuId::new(logical_cpu_id)) else {
                warn!("GICv3 notify_cpu: missing raw CPU id for logical CPU {logical_cpu_id}");
                return;
            };
            let affinity = Affinity::from_mpidr(raw_cpu_id.as_usize() as u64);
            let target = SGITarget::list([affinity]);
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), target);
        }
        TargetCpu::AllButSelf { .. } => {
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::All);
        }
    }
    // `isb` forces the SGI system-register write to be observed as issued
    // before subsequent instructions continue, matching Linux's GICv3
    // SGI-send ordering.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}
