use core::arch::asm;
#[cfg(feature = "pmr")]
use core::sync::atomic::{AtomicBool, Ordering};

use aarch64_cpu::registers::{DAIF, Readable};
#[cfg(feature = "gicv2")]
use arm_gic_driver::v2::*;
#[cfg(feature = "gicv3")]
use arm_gic_driver::v3::*;
use kplat::interrupts::{Handler, HandlerTable, TargetCpu};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
const MAX_IRQ_COUNT: usize = 1024;
static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
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
pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GIC set trigger: {} {}", interrupt_id, edge);
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}
pub fn enable(irq: usize, enabled: bool) {
    trace!("GIC set enable: {irq} {enabled}");
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_irq_enable(intid, enabled);
    if !intid.is_private() {
        gic.set_cfg(intid, Trigger::Edge);
    }
}
pub fn register_handler(irq: usize, handler: Handler) -> bool {
    if IRQ_HANDLER_TABLE.register_handler(irq, handler) {
        trace!("reg_handler handler IRQ {irq}");
        enable(irq, true);
        return true;
    }
    warn!("reg_handler handler for IRQ {irq} failed");
    false
}
pub fn unregister_handler(irq: usize) -> Option<Handler> {
    trace!("unreg_handler handler IRQ {irq}");
    enable(irq, false);
    IRQ_HANDLER_TABLE.unregister_handler(irq)
}
#[cfg(feature = "pmr")]
pub fn set_prio(irq: usize, priority: u8) {
    let intid = unsafe { IntId::raw(irq as u32) };
    let gic = GIC.lock();
    gic.set_prio(intid, priority);
}
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
fn get_priority_mask() -> u8 {
    unsafe {
        core::ptr::read_volatile((*GICC_PMR.get_unchecked()) as *const usize as *const u32) as u8
    }
}
#[cfg(feature = "pmr")]
fn open_high_priority_irq_mode() {
    set_prio_mask(0x80);
    unsafe { asm!("msr daifclr, #2") };
}
#[cfg(feature = "pmr")]
fn close_irq_and_restore_masking() {
    unsafe { asm!("msr daifset, #2") };
    set_prio_mask(0xff);
}
#[cfg(feature = "gicv2")]
#[allow(unused_variables)]
pub fn dispatch_irq_irq(_unused: usize, pmu_irq: usize) -> Option<usize> {
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
#[cfg(feature = "gicv3")]
pub fn dispatch_irq_irq(_unused: usize) -> Option<usize> {
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
#[cfg(feature = "gicv2")]
pub fn init_gic(gicd_base: kplat::memory::VirtAddr, gicc_base: kplat::memory::VirtAddr) {
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
#[cfg(feature = "gicv3")]
pub fn init_gic(gicd_base: kplat::memory::VirtAddr, gicr_base: kplat::memory::VirtAddr) {
    info!("Initialize GICv3...");
    let gicd_base = VirtAddr::new(gicd_base.into());
    let gicr_base = VirtAddr::new(gicr_base.into());
    let mut gic = unsafe { Gic::new(gicd_base, gicr_base) };
    gic.init();
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}
#[cfg(feature = "gicv2")]
pub fn init_gicc() {
    debug!("Initialize GIC CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    cpu.init_current_cpu();
    cpu.set_eoi_mode_ns(false);
}
#[cfg(feature = "gicv3")]
pub fn init_gicr() {
    debug!("Initialize GIC CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    let _ = cpu.init_current_cpu();
    cpu.set_eoi_mode(false);
}
#[cfg(feature = "gicv2")]
pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Self_ => {
            GIC.lock()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::Current);
        }
        TargetCpu::Specific(cpu_id) => {
            let target_list = TargetList::new(&mut [cpu_id].into_iter());
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
#[cfg(feature = "gicv3")]
pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Current { cpu_id: _ } => {
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::current());
        }
        TargetCpu::Other { cpu_id } => {
            let affinity = Affinity::from_mpidr(cpu_id as u64);
            let target_list = TargetList::new([affinity]);
            GIC.lock().cpu_interface().send_sgi(
                IntId::sgi(interrupt_id as u32),
                SGITarget::List(target_list),
            );
        }
        TargetCpu::AllExceptCurrent {
            cpu_id: _,
            cpu_num: _,
        } => {
            GIC.lock()
                .cpu_interface()
                .send_sgi(IntId::sgi(interrupt_id as u32), SGITarget::All);
        }
    }
}
#[cfg(not(feature = "pmr"))]
#[inline]
pub fn enable_local() {
    unsafe { asm!("msr daifclr, #2") };
}
#[cfg(not(feature = "pmr"))]
#[inline]
pub fn disable_local() {
    unsafe { asm!("msr daifset, #2") };
}
#[cfg(not(feature = "pmr"))]
#[inline]
pub fn is_enabled() -> bool {
    !DAIF.matches_all(DAIF::I::Masked)
}
#[cfg(not(feature = "pmr"))]
#[inline]
pub fn save_disable() -> usize {
    let flags: usize;
    unsafe { asm!("mrs {}, daif", out(reg) flags) };
    disable_local();
    flags
}
#[cfg(not(feature = "pmr"))]
#[inline]
pub fn restore(flags: usize) {
    unsafe { asm!("msr daif, {}", in(reg) flags) };
}
#[cfg(feature = "pmr")]
#[inline]
pub fn enable_local() {
    set_prio_mask(0xff);
    unsafe { asm!("msr daifclr, #2") };
}
#[cfg(feature = "pmr")]
#[inline]
pub fn disable_local() {
    open_high_priority_irq_mode();
}
#[cfg(feature = "pmr")]
#[inline]
pub fn is_enabled() -> bool {
    !DAIF.matches_all(DAIF::I::Masked) && get_priority_mask() > 0xa0
}
#[cfg(feature = "pmr")]
#[inline]
pub fn save_disable() -> usize {
    if is_gic_initialized() {
        let pmr = get_priority_mask();
        set_prio_mask(0x80);
        pmr as usize
    } else {
        let flags: usize;
        unsafe { asm!("mrs {}, daif; msr daifset, #2", out(reg) flags) };
        flags
    }
}
#[cfg(feature = "pmr")]
#[inline]
pub fn restore(flags: usize) {
    if is_gic_initialized() {
        set_prio_mask(flags as u8);
    } else {
        unsafe { asm!("msr daif, {}", in(reg) flags) };
    }
}
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! irq_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::interrupts::IntrManager for $name {
            fn enable(irq: usize, enabled: bool) {
                $crate::gic::enable(irq, enabled);
            }

            fn reg_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
                $crate::gic::register_handler(irq, handler)
            }

            fn unreg_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
                $crate::gic::unregister_handler(irq)
            }

            fn dispatch_irq(irq: usize) -> Option<usize> {
                let pmu_irq = crate::config::devices::PMU_IRQ;
                $crate::gic::dispatch_irq_irq(irq, pmu_irq)
            }

            fn notify_cpu(interrupt_id: usize, target: kplat::interrupts::TargetCpu) {
                $crate::gic::notify_cpu(interrupt_id, target);
            }

            fn set_prio(irq: usize, priority: u8) {
                $crate::gic::set_prio(irq, priority);
            }

            fn save_disable() -> usize {
                $crate::gic::save_disable()
            }

            fn restore(flag: usize) {
                $crate::gic::restore(flag);
            }

            fn enable_local() {
                $crate::gic::enable_local();
            }

            fn disable_local() {
                $crate::gic::disable_local();
            }

            fn is_enabled() -> bool {
                $crate::gic::is_enabled()
            }
        }
    };
}
