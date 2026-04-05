// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use arm_gic_driver::v3::*;
use kplat::interrupts::{Handler, HandlerTable, TargetCpu};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::VirtAddr as MemVirtAddr;

const MAX_IRQ_COUNT: usize = 1024;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();

/// Configure the trigger type for an interrupt line.
pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv3 set trigger: {} {}", interrupt_id, edge);
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

/// Enable or disable a GIC interrupt.
pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv3 set enable: {irq} {enabled}");
    let intid = unsafe { IntId::raw(irq as u32) };
    let mut gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
    if !intid.is_private() {
        gic.set_cfg(intid, Trigger::Edge);
    }
}

/// Register an IRQ handler and enable the line if successful.
pub fn register_handler(irq: usize, handler: Handler) -> bool {
    if IRQ_HANDLER_TABLE.register_handler(irq, handler) {
        trace!("reg_handler handler IRQ {irq}");
        enable(irq, true);
        return true;
    }
    warn!("reg_handler handler for IRQ {irq} failed");
    false
}

/// Unregister an IRQ handler and disable the line.
pub fn unregister_handler(irq: usize) -> Option<Handler> {
    trace!("unreg_handler handler IRQ {irq}");
    enable(irq, false);
    IRQ_HANDLER_TABLE.unregister_handler(irq)
}

/// Set the priority for an interrupt line.
pub fn set_prio(_irq: usize, _priority: u8) {
    unreachable!()
}

/// Dispatch an IRQ on GICv3 and return the acknowledged IRQ number.
pub fn dispatch_irq(_unused: usize, _pmu_irq: usize) -> Option<usize> {
    let ack = TRAP_OP.ack1();
    if ack.is_special() {
        return None;
    }
    trace!("Handling IRQ: {ack:?}");
    if !IRQ_HANDLER_TABLE.handle(ack.to_u32() as _) {
        warn!("Undispatch_irqd IRQ {:?}", ack);
    }
    TRAP_OP.eoi1(ack);
    if TRAP_OP.eoi_mode() {
        TRAP_OP.dir(ack);
    }
    Some(ack.to_u32() as usize)
}

/// Initialize the GICv3 distributor and redistributor.
pub fn init_global(gicd_base: MemVirtAddr, gicr_base: MemVirtAddr) {
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

/// Initialize the GICv3 CPU interface for the current core.
pub fn init_current_cpu() {
    debug!("Initialize GICv3 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    let _ = cpu.init_current_cpu();
    cpu.set_eoi_mode(false);
}

/// Send a software interrupt to a target CPU.
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
