// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv3 backend.

use arm_gic_driver::v3::*;
use khal::irq::TargetCpu;
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
    cpu.set_eoi_mode(false);
}

pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv3 set trigger: {interrupt_id} {edge}");
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv3 set enable: {irq} {enabled}");
    let intid = unsafe { IntId::raw(irq as u32) };
    let mut gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
}

pub fn set_prio(irq: usize, priority: u8) {
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

pub fn dispatch_irq() -> Option<usize> {
    let ack = TRAP_OP.ack1();
    if ack.is_special() {
        return None;
    }
    let irq = ack.to_u32() as usize;
    TRAP_OP.eoi1(ack);
    if TRAP_OP.eoi_mode() {
        TRAP_OP.dir(ack);
    }
    Some(irq)
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Self_ => {
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::current());
        }
        TargetCpu::Specific(cpu_id) => {
            let affinity = Affinity::from_mpidr(cpu_id as u64);
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
}
