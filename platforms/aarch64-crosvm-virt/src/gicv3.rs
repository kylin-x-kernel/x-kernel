//! GICv3 initialization and IRQ routing helpers.
use core::{
    arch::asm,
    sync::atomic::{AtomicBool, Ordering},
};

use aarch64_cpu::registers::*;
use arm_gic::gicv3::*;
use kplat::{
    interrupts::{Handler, HandlerTable},
    memory::VirtAddr,
};
use kspin::SpinNoIrq;
use log::*;

use crate::config::plat::CPU_NUM;
static GICD_INIT: AtomicBool = AtomicBool::new(false);
const MAX_IRQ_COUNT: usize = 1024;
static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();
struct GicV3Wrapper {
    inner: GicV3,
}
unsafe impl Send for GicV3Wrapper {}
unsafe impl Sync for GicV3Wrapper {}
static GIC_V3S: [SpinNoIrq<Option<GicV3Wrapper>>; CPU_NUM] =
    [const { SpinNoIrq::new(None) }; CPU_NUM];
/// Read the current CPU ID from `MPIDR_EL1`.
#[inline]
fn get_current_cpu_id() -> usize {
    let mpidr_el1: usize;
    unsafe {
        core::arch::asm!("mrs {}, MPIDR_EL1", out(reg) mpidr_el1);
    }
    mpidr_el1 & 0xff
}
/// Initialize the per-CPU GICv3 interface and distributor.
pub fn init_gic(gicd_base: VirtAddr, gicr_base: VirtAddr) {
    info!(
        "Initialize GICv3... from 0x{:x} 0x{:x}",
        gicd_base.as_usize(),
        gicr_base.as_usize()
    );
    const GICR_RD_OFFSET: usize = 0x20000;
    const GICR_TYPER_HI_OFFSET: usize = 0x0008;
    let mut gic_v3_lock = GIC_V3S[get_current_cpu_id()].lock();
    let mpidr_aff: u64 = aarch64_cpu::registers::MPIDR_EL1.get() & 0xffffff;
    let mut cur_gicr_base: usize = gicr_base.as_usize();
    loop {
        let gicr_typer_aff: u64 = unsafe {
            core::ptr::read_volatile((cur_gicr_base + GICR_TYPER_HI_OFFSET) as *const u64)
        };
        trace!("gicr_typer_aff: 0x{:x?}", gicr_typer_aff);
        if mpidr_aff == gicr_typer_aff >> 32 {
            info!("cur_gicr_base: 0x{:x}", cur_gicr_base);
            break;
        }
        cur_gicr_base += GICR_RD_OFFSET;
    }
    let mut v3: GicV3 = unsafe { GicV3::new(gicd_base.as_mut_ptr_of(), cur_gicr_base as *mut u64) };
    if !GICD_INIT.load(Ordering::SeqCst) {
        v3.setup();
        GICD_INIT.store(true, Ordering::SeqCst);
    }
    v3.init_cpu();
    *gic_v3_lock = Some(GicV3Wrapper { inner: v3 });
}
/// Configure an interrupt as edge or level triggered.
pub fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GIC set trigger: {}  edge: {}", interrupt_id, edge);
    let mut gic_v3_lock = GIC_V3S[get_current_cpu_id()].lock();
    let gic_v3 = &mut gic_v3_lock.as_mut().unwrap().inner;
    let intid = IntId::from(interrupt_id as u32);
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    gic_v3.set_trigger(intid, cfg);
}
/// Enable or disable a specific interrupt.
pub fn enable(interrupt_id: usize, enabled: bool) {
    trace!("GIC set enable: {} {}", interrupt_id, enabled);
    let mut gic_v3_lock = GIC_V3S[get_current_cpu_id()].lock();
    let gic_v3 = &mut gic_v3_lock.as_mut().unwrap().inner;
    if enabled {
        gic_v3.enable_interrupt(IntId::from(interrupt_id as u32), true);
    } else {
        gic_v3.enable_interrupt(IntId::from(interrupt_id as u32), false);
    }
}
/// Register an IRQ handler and enable the interrupt.
pub fn reg_handler_handler(interrupt_id: usize, handler: Handler) -> bool {
    if IRQ_HANDLER_TABLE.register_handler(interrupt_id, handler) {
        trace!("reg_handler handler IRQ {}", interrupt_id);
        enable(interrupt_id, true);
        return true;
    }
    false
}
/// Unregister an IRQ handler and disable the interrupt.
pub fn unreg_handler_handler(interrupt_id: usize) -> Option<Handler> {
    trace!("unreg_handler handler IRQ {}", interrupt_id);
    enable(interrupt_id, false);
    IRQ_HANDLER_TABLE.unregister_handler(interrupt_id)
}
fn end_of_interrupt(irq: usize) {
    GicV3::end_interrupt(IntId::from(irq as u32));
}
fn get_and_acknowledge_interrupt() -> usize {
    let irq = u32::from(GicV3::get_and_acknowledge_interrupt().unwrap()) as usize;
    return irq;
}
/// Send a software-generated interrupt to target CPUs.
pub fn notify_cpu(irq: usize, target: kplat::interrupts::TargetCpu) {
    use arm_gic::gicv3::SgiTarget;
    let sgi_intid = IntId::from(irq as u32);
    match target {
        kplat::interrupts::TargetCpu::AllButSelf { .. } => {
            GicV3::send_sgi(sgi_intid, SgiTarget::All);
        }
        _ => {
            unimplemented!();
        }
    }
}
#[allow(dead_code)]
fn test_manual_trigger() {
    let gicd_base = 0xffff00003fff0000 as usize;
    info!("=== Manual Trigger Test ===");
    unsafe {
        core::ptr::write_volatile((gicd_base + 0x200 + 1 * 4) as *mut u32, 0x1);
        let ispendr = core::ptr::read_volatile((gicd_base + 0x200 + 1 * 4) as *const u32);
        info!("Manual trigger: ISPENDR = {:#x}", ispendr);
    }
    for _ in 0..1000 {
        core::hint::spin_loop();
    }
    info!("Did interrupt fire? Check handler logs");
}
#[allow(dead_code)]
fn debug_irq_32() {
    let irq = 32;
    let gicd_base = 0xffff00003fff0000 as usize;
    unsafe {
        let isenabler =
            core::ptr::read_volatile((gicd_base + 0x100 + (irq / 32) * 4) as *const u32);
        info!(
            "GICD_ISENABLER[1]: {:#x}, bit 0: {}",
            isenabler,
            (isenabler >> (irq % 32)) & 1
        );
        let ispendr = core::ptr::read_volatile((gicd_base + 0x200 + (irq / 32) * 4) as *const u32);
        info!(
            "GICD_ISPENDR[1]: {:#x}, bit 0: {}",
            ispendr,
            (ispendr >> (irq % 32)) & 1
        );
        let ipriorityr = core::ptr::read_volatile((gicd_base + 0x400 + irq) as *const u8);
        info!("GICD_IPRIORITYR[32]: {:#x}", ipriorityr);
        let irouter = core::ptr::read_volatile((gicd_base + 0x6000 + irq * 8) as *const u64);
        info!("GICD_IROUTER[32]: {:#x}", irouter);
        let gicd_ctlr = core::ptr::read_volatile(gicd_base as *const u32);
        info!(
            "GICD_CTLR: {:#x} (EnableGrp0:{}, EnableGrp1:{})",
            gicd_ctlr,
            gicd_ctlr & 1,
            (gicd_ctlr >> 1) & 1
        );
        let igroupr = core::ptr::read_volatile((gicd_base + 0x80 + (irq / 32) * 4) as *const u32);
        info!(
            "GICD_IGROUPR[1]: {:#x}, bit 0: {} (0=Group0, 1=Group1)",
            igroupr,
            (igroupr >> (irq % 32)) & 1
        );
        let igrpmodr = core::ptr::read_volatile((gicd_base + 0xD00 + (irq / 32) * 4) as *const u32);
        info!(
            "GICD_IGRPMODR[1]: {:#x}, bit 0: {}",
            igrpmodr,
            (igrpmodr >> (irq % 32)) & 1
        );
        let icfgr = core::ptr::read_volatile((gicd_base + 0xC00 + (irq / 16) * 4) as *const u32);
        let cfg_shift = ((irq % 16) * 2) + 1;
        info!(
            "GICD_ICFGR[2]: {:#x}, bit {}: {} (0=level, 1=edge)",
            icfgr,
            cfg_shift,
            (icfgr >> cfg_shift) & 1
        );
        let isactiver =
            core::ptr::read_volatile((gicd_base + 0x300 + (irq / 32) * 4) as *const u32);
        info!(
            "GICD_ISACTIVER[1]: {:#x}, bit 0: {}",
            isactiver,
            (isactiver >> (irq % 32)) & 1
        );
        info!("=== Write Test ===");
        let old_val = core::ptr::read_volatile((gicd_base + 0x100 + (irq / 32) * 4) as *const u32);
        core::ptr::write_volatile((gicd_base + 0x100 + (irq / 32) * 4) as *mut u32, 0x1);
        let new_val = core::ptr::read_volatile((gicd_base + 0x100 + (irq / 32) * 4) as *const u32);
        info!(
            "Write 0x1 to ISENABLER[1]: before={:#x}, after={:#x}",
            old_val, new_val
        );
        let mut pmr: u64;
        let mut igrpen1: u64;
        let mut ctlr: u64;
        core::arch::asm!(
            "mrs {0}, S3_0_C4_C6_0",
            "mrs {1}, S3_0_C12_C12_7",
            "mrs {2}, S3_0_C12_C12_4",
            out(reg) pmr,
            out(reg) igrpen1,
            out(reg) ctlr,
        );
        info!("ICC_PMR_EL1: {:#x}", pmr);
        info!("ICC_IGRPEN1_EL1: {:#x}", igrpen1);
        info!("ICC_CTLR_EL1: {:#x}", ctlr);
        let mpidr = aarch64_cpu::registers::MPIDR_EL1.get();
        info!("Current CPU MPIDR: {:#x}", mpidr & 0xffffff);
    }
}
pub fn dispatch_irq_irq(_unused: usize) -> Option<usize> {
    let irq = get_and_acknowledge_interrupt();
    if !IRQ_HANDLER_TABLE.handle(irq as u32 as _) {
        debug!("Undispatch_irqd IRQ {:?}", irq);
    }
    if irq <= 1019 {
        end_of_interrupt(irq);
    }
    Some(irq)
}
#[inline]
pub fn enable_local() {
    unsafe { asm!("msr daifclr, #2") };
}
#[inline]
pub fn disable_local() {
    unsafe { asm!("msr daifset, #2") };
}
#[inline]
pub fn is_enabled() -> bool {
    !DAIF.matches_all(DAIF::I::Masked)
}
#[inline]
pub fn save_disable() -> usize {
    let flags: usize;
    unsafe { asm!("mrs {}, daif", out(reg) flags) };
    disable_local();
    flags
}
#[inline]
pub fn restore(flags: usize) {
    unsafe { asm!("msr daif, {}", in(reg) flags) };
}
#[macro_export]
macro_rules! irq_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::interrupts::IntrManager for $name {
            fn enable(irq: usize, enabled: bool) {
                $crate::gicv3::enable(irq, enabled);
            }

            fn reg_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
                $crate::gicv3::reg_handler_handler(irq, handler)
            }

            fn unreg_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
                $crate::gicv3::unreg_handler_handler(irq)
            }

            fn dispatch_irq(irq: usize) -> Option<usize> {
                $crate::gicv3::dispatch_irq_irq(irq)
            }

            fn notify_cpu(interrupt_id: usize, target: kplat::interrupts::TargetCpu) {
                $crate::gicv3::notify_cpu(interrupt_id, target);
            }

            fn set_prio(_irq: usize, _priority: u8) {
                todo!()
            }

            fn save_disable() -> usize {
                $crate::gicv3::save_disable()
            }

            fn restore(flag: usize) {
                $crate::gicv3::restore(flag);
            }

            fn enable_local() {
                $crate::gicv3::enable_local();
            }

            fn disable_local() {
                $crate::gicv3::disable_local();
            }

            fn is_enabled() -> bool {
                $crate::gicv3::is_enabled()
            }
        }
    };
}
