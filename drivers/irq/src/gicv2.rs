// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv2 backend.

#[cfg(feature = "pmr")]
use core::arch::asm;
#[cfg(feature = "pmr")]
use core::sync::atomic::{AtomicBool, Ordering};

use arm_gic_driver::v2::*;
use khal::irq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
#[cfg(feature = "pmr")]
static GICC_PMR: LazyInit<usize> = LazyInit::new();
#[cfg(feature = "pmr")]
const PMR_OFFSET: usize = 0x4;
#[cfg(feature = "pmr")]
static GIC_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "pmr")]
#[inline]
pub fn set_gic_init_status(status: bool) {
    GIC_INITIALIZED.store(status, Ordering::SeqCst);
}

#[cfg(feature = "pmr")]
#[inline]
pub fn is_gic_initialized() -> bool {
    GIC_INITIALIZED.load(Ordering::SeqCst)
}

pub fn init(gicd_base: memaddr::VirtAddr, gicc_base: memaddr::VirtAddr) {
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
    // SAFETY: both distributor and CPU interface bases come from platform
    // discovery and point at the mapped GICv2 MMIO frames for this machine.
    let mut gic = unsafe { Gic::new(gicd_base, gicc_base, None) };
    gic.init();
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

#[cfg(feature = "pmr")]
fn set_prio_mask(priority: u8) {
    // SAFETY: `GICC_PMR` is initialized from the mapped CPU-interface frame
    // before PMR mode is enabled, and PMR is a single 32-bit MMIO register.
    unsafe {
        core::ptr::write_volatile((*GICC_PMR.get_unchecked()) as *mut u32, priority as u32);
    }
}

#[cfg(feature = "pmr")]
fn open_high_priority_irq_mode() {
    set_prio_mask(0x80);
    // SAFETY: writing `daifclr, #2` only unmasks IRQ delivery on the current
    // CPU and does not touch memory.
    unsafe { asm!("msr daifclr, #2") };
}

#[cfg(feature = "pmr")]
fn close_irq_and_restore_masking() {
    // SAFETY: writing `daifset, #2` only masks IRQ delivery on the current
    // CPU and does not touch memory.
    unsafe { asm!("msr daifset, #2") };
    set_prio_mask(0xff);
}

pub fn dispatch_irq(_pmu_irq: usize) -> Option<(usize, usize)> {
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
    // SAFETY: `isb` only orders the acknowledged interrupt before the handler.
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) };

    #[cfg(feature = "nmi-pmu")]
    if irq != _pmu_irq {
        open_high_priority_irq_mode();
    }
    Some((irq, u32::from(ack) as usize))
}

pub fn complete_irq(completion_cookie: usize) {
    let ack = Ack::from(completion_cookie as u32);
    TRAP_OP.dir(ack);

    #[cfg(feature = "nmi-pmu")]
    {
        let hwirq = match ack {
            Ack::Other(intid) => intid,
            Ack::SGI { intid, cpu_id: _ } => intid,
        }
        .to_u32() as usize;
        if hwirq != kbuild_config::PMU_IRQ {
            close_irq_and_restore_masking();
        }
    }
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    // SAFETY: `dmb ishst` orders the caller's prior Normal-memory stores
    // before the SGI MMIO write, which is required for cross-CPU publish
    // before notify semantics.
    unsafe { core::arch::asm!("dmb ishst", options(nomem, nostack)) };
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
