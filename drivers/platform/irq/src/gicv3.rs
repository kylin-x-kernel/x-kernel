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

/// Return whether the GIC advertises GICv3.3 NMI attribute support.
///
/// This is a read-only capability query backed by `GICD_TYPER.NMI`; it never
/// modifies a live interrupt attribute as part of feature detection.
#[cfg(feature = "nmi-hardware")]
pub fn supports_hardware_nmi() -> bool {
    GIC.lock().supports_nmi_attributes()
}

/// Set or clear the GICv3.3 NMI attribute for `irq`.
///
/// Delegates to `arm-gic-driver`; returns `false` when the line cannot be
/// programmed (unsupported INTID, or the current CPU's redistributor frame
/// cannot be located).
#[cfg(feature = "nmi-hardware")]
pub fn set_nmi_attr(irq: usize, nmi: bool) -> bool {
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let attribute = if nmi {
        NmiAttribute::NonMaskable
    } else {
        NmiAttribute::Maskable
    };
    match GIC.lock().set_nmi_attribute(intid, attribute) {
        Ok(()) => true,
        Err(e) => {
            warn!("set_nmi_attr: {e}; hwirq {irq}");
            false
        }
    }
}

/// Ack an interrupt and return `(hwirq, cookie, is_nmi)`.
///
/// Does **not** open the pseudo‑NMI window; that is the caller's decision.
fn ack_irq() -> Option<(usize, usize, bool)> {
    let ack = TRAP_OP.ack1();
    if ack.is_special() {
        // IAR1 never returns an NMI-attribute interrupt: when an NMI is
        // pending, IAR1 returns a special INTID instead. In hardware-NMI mode
        // the real ack happens via ICC_NMIAR1_EL1, after which the GIC selects
        // kirq's generic NMI entry directly.
        #[cfg(feature = "nmi-hardware")]
        if karch::allint_active() {
            let intid = read_nmi_iar();
            if intid < 1020 {
                // SAFETY: NMIAR1 returned a valid (non-special) INTID.
                let nmi_ack = unsafe { IntId::raw(intid) };
                TRAP_OP.eoi1(nmi_ack);
                // SAFETY: `isb` only orders the acknowledged interrupt
                // before the handler.
                unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
                return Some((intid as usize, intid as usize, true));
            }
        }
        // Truly spurious: neither IAR1 nor (in hardware mode) NMIAR1 has a
        // pending interrupt to dispatch.
        return None;
    }
    let irq = ack.to_u32() as usize;

    // After ack, running priority reflects this interrupt's priority.  NMI
    // sources are programmed at priority 0.  The RPR read only decides
    // whether to open the NMI window, which exists solely under `nmi-pseudo`;
    // skip it otherwise to keep the IRQ hot path free of system-register
    // traffic.
    #[cfg(feature = "nmi-pseudo")]
    let is_nmi = {
        let rpr: u64;
        // SAFETY: reading ICC_RPR_EL1, a read-only system register.
        unsafe { core::arch::asm!("mrs {}, ICC_RPR_EL1", out(reg) rpr, options(nomem, nostack)) };
        rpr == 0
    };
    #[cfg(not(feature = "nmi-pseudo"))]
    let is_nmi = false;

    TRAP_OP.eoi1(ack);
    // Order the acknowledged interrupt before the handler.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    Some((irq, irq, is_nmi))
}

/// Open the pseudo‑NMI window for the IRQ-on path.
///
/// Sets PMR to [`pmr::NMI_ONLY`] so only pseudo‑NMIs can preempt
/// the handler, then clears `DAIF.I` so those NMIs are delivered.
/// PMR is restored by the exception exit path (saved at entry).
///
/// This is used before a normal IRQ handler, or after an IRQ-on NMI handler has
/// completed. It is never used by the dedicated IRQ-off NMI entry.
#[cfg(feature = "nmi-pseudo")]
fn open_pseudo_nmi_window_from_irqson() {
    assert!(karch::pmr::is_ready());
    karch::pmr::write(karch::pmr::NMI_ONLY);
    // SAFETY: exception entry set DAIF.I; clear it so pseudo‑NMI can nest
    // while normal IRQs remain gated by PMR. `nostack` describes the
    // instruction, but `nomem` is deliberately omitted: opening NMI delivery
    // must retain the compiler's default memory clobber across this boundary.
    unsafe { core::arch::asm!("msr daifclr, #2", options(nostack)) };
}

/// IRQ-on path: ack, classify, and early EOI.
///
/// Under `nmi-pseudo`, a non‑NMI interrupt opens the NMI window (PMR
/// lowered to `NMI_ONLY`) so a real pseudo‑NMI can preempt the handler; the
/// RPR read that drives this lives in [`ack_irq`].  Under `nmi-hardware`, an
/// NMI-attribute interrupt that fired while IRQs were enabled is acked via
/// `ICC_NMIAR1_EL1` in [`ack_irq`] and dispatched directly through kirq's NMI
/// helper. For a normal interrupt in hardware mode, the ALLINT mask set by
/// exception entry is cleared before the generic IRQ handler so a
/// Superpriority NMI can preempt it; ordinary IRQs stay masked by `DAIF.I`.
pub(super) fn dispatch_irq_from_irqson() -> Option<kirq::Virq> {
    let Some((irq, cookie, is_nmi)) = ack_irq() else {
        // The exception came from an IRQ-on context. Even if the pending
        // interrupt retired before the acknowledge, restore the IRQ-on NMI
        // window before the common outer IRQ-exit tail, as Linux does after a
        // special INTID from IAR.
        unmask_nmis_from_irqson();
        return None;
    };

    if is_nmi {
        kirq::generic_handle_nmi(kirq::DispatchedIrq::new(irq, cookie));
    }

    // Linux does this after completing an IRQ-on NMI and before dispatching a
    // normal IRQ. It admits another NMI into the remaining outer IRQ context
    // without admitting normal IRQs.
    unmask_nmis_from_irqson();

    if is_nmi {
        None
    } else {
        kirq::generic_handle_irq(kirq::PendingIrq::new(
            kirq::IrqRef::Domain(kirq::GIC_ROOT_DOMAIN, irq),
            cookie,
        ))
    }
}

fn unmask_nmis_from_irqson() {
    #[cfg(feature = "nmi-pseudo")]
    open_pseudo_nmi_window_from_irqson();
    // `allint_clear` is a no-op until runtime hardware-NMI initialization has
    // enabled the mechanism on this CPU.
    #[cfg(feature = "nmi-hardware")]
    karch::allint_clear();
}

/// NMI path: an IRQ exception taken while IRQs were masked.
///
/// Under `nmi-pseudo`, PMR is lowered to `NMI_ONLY` around the IAR1 ack so
/// the window for a preempting pseudo‑NMI stays closed.  Under
/// `nmi-hardware`, the NMI is acknowledged via `ICC_NMIAR1_EL1` (IAR1 never
/// returns NMI-attribute interrupts).  Mirrors
/// `__gic_handle_irq_from_irqsoff`.
pub fn dispatch_irq_from_irqsoff() -> Option<(usize, usize)> {
    #[cfg(feature = "nmi-pseudo")]
    let saved_pmr = karch::pmr::read();
    #[cfg(feature = "nmi-pseudo")]
    {
        karch::pmr::write(karch::pmr::NMI_ONLY);
        // Ensure the PMR write is visible before the IAR read.
        aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
    }

    // Hardware NMI must be acknowledged via ICC_NMIAR1_EL1 — IAR1 never
    // returns NMI-attribute interrupts.  NMIAR1 is only accessible when
    // SCTLR_EL1.NMI=1, which holds in the compile-time hardware mode; in a
    // pseudo-only build the NMI path keeps using IAR1.
    let ack = {
        #[cfg(feature = "nmi-hardware")]
        {
            if karch::allint_active() {
                match read_nmi_iar() {
                    // SAFETY: NMIAR1 returned a valid (non-special) INTID.
                    intid if intid < 1020 => Some(unsafe { IntId::raw(intid) }),
                    // Spurious: NMIAR1 returned a special INTID.  IAR1 cannot
                    // recover the pending NMI either — it returns 1022 while
                    // an NMI is pending — so an ack1() fallback is useless.
                    _ => None,
                }
            } else {
                // Degraded build (runtime NMI disabled): the exception is a
                // plain IRQ, so acknowledge via IAR1.
                Some(TRAP_OP.ack1())
            }
        }
        #[cfg(not(feature = "nmi-hardware"))]
        {
            Some(TRAP_OP.ack1())
        }
    };
    let result = match ack {
        Some(ack) if !ack.is_special() => {
            // Ack without re-reading ICC_RPR_EL1: the caller already knows
            // this is an NMI, and the read would otherwise be duplicated on
            // every pseudo-NMI.
            let irq = ack.to_u32() as usize;
            TRAP_OP.eoi1(ack);
            // Order the acknowledged interrupt before the handler.
            aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
            Some((irq, irq))
        }
        _ => None,
    };

    #[cfg(feature = "nmi-pseudo")]
    karch::pmr::write(saved_pmr);

    result
}

/// Acknowledge the highest-priority pending Group 1 hardware NMI.
///
/// A valid read of `ICC_NMIAR1_EL1` acknowledges the interrupt, transitions
/// it to the active state, and returns its INTID. If no eligible NMI remains
/// pending, the register returns a special INTID, which callers reject by
/// requiring `intid < 1020`.
///
/// This register may only be accessed after runtime hardware-NMI
/// initialization has enabled `SCTLR_EL1.NMI`; otherwise the access is
/// architecturally UNDEFINED.
#[cfg(feature = "nmi-hardware")]
fn read_nmi_iar() -> u32 {
    let value: u64;
    // SAFETY: both call sites first require `karch::allint_active()`, and are
    // reached only after platform initialization has validated FEAT_NMI and
    // GICv3.3 NMI support and enabled `SCTLR_EL1.NMI` on this CPU. The read
    // acknowledges the NMI in the GIC but accesses neither memory nor the
    // stack, matching `nomem` and `nostack`.
    //
    // The current assembler target does not accept the symbolic
    // `ICC_NMIAR1_EL1` name, so use its architectural system-register
    // encoding: op0=3, op1=0, CRn=12, CRm=9, op2=5.
    unsafe { core::arch::asm!("mrs {}, s3_0_c12_c9_5", out(reg) value, options(nomem, nostack)) };
    // ICC_NMIAR1_EL1.INTID occupies bits [23:0]; bits [63:24] are RES0.
    (value as u32) & 0x00FF_FFFF
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
