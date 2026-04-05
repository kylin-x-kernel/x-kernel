// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(feature = "pmr")]
use core::arch::asm;
#[cfg(feature = "pmr")]
use core::sync::atomic::{AtomicBool, Ordering};

use arm_gic_driver::v2::*;
use kplat::interrupts::{Handler, HandlerTable, TargetCpu};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::VirtAddr as MemVirtAddr;

const MAX_IRQ_COUNT: usize = 1024;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
#[cfg(feature = "pmr")]
static GICC_PMR: LazyInit<usize> = LazyInit::new();
#[cfg(feature = "pmr")]
#[allow(dead_code)]
const PMR_OFFSET: usize = 0x4;
#[cfg(feature = "pmr")]
static GIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Update the PMR-init status for early paths that need it.
#[cfg(feature = "pmr")]
#[inline]
pub fn set_gic_init_status(status: bool) {
    GIC_INITIALIZED.store(status, Ordering::SeqCst);
}

/// Query whether the GIC PMR has been initialized.
#[cfg(feature = "pmr")]
#[inline]
pub fn is_gic_initialized() -> bool {
    GIC_INITIALIZED.load(Ordering::SeqCst)
}

/// Configure the trigger type for an interrupt line.
pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv2 set trigger: {} {}", interrupt_id, edge);
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

/// Enable or disable a GIC interrupt.
pub fn enable(irq: usize, enabled: bool) {
    trace!("GICv2 set enable: {irq} {enabled}");
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
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
#[cfg(feature = "pmr")]
pub fn set_prio(irq: usize, priority: u8) {
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_priority(intid, priority);
}

/// Priority setting is unavailable without PMR support.
#[cfg(not(feature = "pmr"))]
pub fn set_prio(_irq: usize, _priority: u8) {
    unreachable!()
}

#[cfg(feature = "pmr")]
fn set_prio_mask(priority: u8) {
    unsafe {
        core::ptr::write_volatile((*GICC_PMR.get_unchecked()) as *mut u32, priority as u32);
    }
}

#[cfg(feature = "pmr")]
fn open_high_priority_irq_mode() {
    set_prio_mask(0x80);
    unsafe { asm!("msr daifclr, #2") };
}

#[cfg(feature = "pmr")]
#[allow(dead_code)]
fn close_irq_and_restore_masking() {
    unsafe { asm!("msr daifset, #2") };
    set_prio_mask(0xff);
}

/// Dispatch an IRQ on GICv2 and return the acknowledged IRQ number.
#[allow(unused_variables)]
pub fn dispatch_irq(_unused: usize, pmu_irq: usize) -> Option<usize> {
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
    #[cfg(feature = "nmi-pmu")]
    if irq != pmu_irq {
        open_high_priority_irq_mode();
    }
    if !IRQ_HANDLER_TABLE.handle(irq) {
        debug!("Undispatch_irqd IRQ {ack:?}");
    }
    TRAP_OP.eoi(ack);
    if TRAP_OP.eoi_mode_ns() {
        TRAP_OP.dir(ack);
    }
    #[cfg(feature = "nmi-pmu")]
    if irq != pmu_irq {
        close_irq_and_restore_masking();
    }
    Some(irq)
}

/// Initialize the GICv2 distributor and CPU interface.
pub fn init_global(gicd_base: MemVirtAddr, gicc_base: MemVirtAddr) {
    if GIC.get().is_some() {
        return;
    }
    info!("Initialize GICv2...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicc_base = VirtAddr::new(gicc_base.into());
    #[cfg(feature = "pmr")]
    {
        GICC_PMR.init_once(usize::from(gicc_base) + PMR_OFFSET);
        set_gic_init_status(true);
    }
    let mut gic = unsafe { Gic::new(gicd_base, gicc_base, None) };
    gic.init();
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}

/// Initialize the GICv2 CPU interface for the current core.
pub fn init_current_cpu() {
    debug!("Initialize GICv2 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    cpu.init_current_cpu();
    cpu.set_eoi_mode_ns(false);
}

/// Send a software interrupt to a target CPU.
pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
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
