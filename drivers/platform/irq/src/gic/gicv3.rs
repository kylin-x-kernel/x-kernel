// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GICv3 backend.

extern crate alloc;

use alloc::sync::Arc;

use arm_gic_driver::v3::*;
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kirq::TargetCpu;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

use super::GicInitError;

static GIC: LazyInit<SpinNoIrq<Gic>> = LazyInit::new();
static TRAP_OP: LazyInit<TrapOp> = LazyInit::new();

mod affinity;
pub(super) use affinity::CHIP as AFFINITY;

#[cfg(any(feature = "nmi-pseudo", feature = "nmi-hardware"))]
#[path = "nmi.rs"]
pub(super) mod nmi;

const IRQ_WORK_SGI: u32 = 15;
static IRQ_WORK_HANDLER_REGISTERED: LazyInit<bool> = LazyInit::new();

unittest::path_mod_test!(config_tests, "../../ut-tests/gicv3_config.rs");

/// Checks private startup configuration or configures an admitted standard SPI.
///
/// Does not sleep; task/IRQ context is allowed, NMI context and callers holding
/// GIC are not. Private reads address the executing CPU, which the caller must
/// select before entry. One GIC guard excludes IRQs and migration throughout
/// register access. Priority and NMI attributes remain caller-owned.
///
/// # Errors
/// Admission and unsupported private trigger changes return `UnsupportedFlow`
/// before mutation. Missing initialization or SPI mask/RWP/configuration/enable
/// restore failure returns `ConfigurationFailed`. SPI failure requests masking;
/// it does not roll back writes, drain handlers or prove hardware quiescence.
pub(super) fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
    let config = checked_config(desc, super::STANDARD_INTID_LIMIT.get().copied())?;
    let gic = GIC
        .get()
        .ok_or(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })?;
    if let Some((intid, trigger)) = config {
        let mut gic = gic.lock();
        configure_locked(&mut *gic, intid, trigger)?;
    }
    Ok(())
}

fn checked_config(
    desc: kirq::IrqDesc,
    limit: Option<usize>,
) -> Result<Option<(IntId, Trigger)>, kirq::IrqDescError> {
    super::select_gic_flow(desc, limit)?;
    if limit.is_none_or(|limit| desc.hwirq >= limit.min(1020)) {
        return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    let trigger = match desc.trigger {
        kirq::IrqTrigger::Unknown(_) => return Ok(None),
        kirq::IrqTrigger::EdgeRising => Trigger::Edge,
        kirq::IrqTrigger::EdgeFalling if desc.hwirq >= 16 => Trigger::Edge,
        kirq::IrqTrigger::LevelHigh | kirq::IrqTrigger::LevelLow if desc.hwirq >= 16 => {
            Trigger::Level
        }
        _ => return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq }),
    };
    // SAFETY: the bound above restricts hwirq to implemented standard INTIDs
    // below 1020, excluding special IDs and truncation before vendor indexing.
    Ok(Some((unsafe { IntId::raw(desc.hwirq as u32) }, trigger)))
}

trait ConfigRegisters {
    fn get_cfg(&mut self, intid: IntId) -> Trigger;
    fn set_cfg(&mut self, intid: IntId, trigger: Trigger);
    fn is_irq_enable(&mut self, intid: IntId) -> bool;
    fn set_irq_enable(&mut self, intid: IntId, enabled: bool);
    fn wait_for_distributor_rwp(&mut self) -> bool;
}

impl ConfigRegisters for Gic {
    fn get_cfg(&mut self, intid: IntId) -> Trigger {
        Gic::get_cfg(self, intid)
    }

    fn set_cfg(&mut self, intid: IntId, trigger: Trigger) {
        Gic::set_cfg(self, intid, trigger);
    }

    fn is_irq_enable(&mut self, intid: IntId) -> bool {
        Gic::is_irq_enable(self, intid)
    }

    fn set_irq_enable(&mut self, intid: IntId, enabled: bool) {
        Gic::set_irq_enable(self, intid, enabled);
    }

    fn wait_for_distributor_rwp(&mut self) -> bool {
        Gic::wait_for_distributor_rwp(self).is_ok()
    }
}

/// Requires one GIC guard across all reads, writes and bounded polling.
fn configure_locked(
    registers: &mut impl ConfigRegisters,
    intid: IntId,
    trigger: Trigger,
) -> Result<(), kirq::IrqDescError> {
    if registers.get_cfg(intid) == trigger {
        return Ok(());
    }
    // Keep fixed private/NMI startup usable, but do not mutate a private source
    // without a Redistributor completion contract. Gic::set_cfg would also
    // change every redistributor, which a local mask cannot protect.
    if intid.is_private() {
        return Err(kirq::IrqDescError::UnsupportedFlow {
            hwirq: intid.to_u32() as usize,
        });
    }
    let was_enabled = registers.is_irq_enable(intid);
    if was_enabled {
        registers.set_irq_enable(intid, false);
    }
    // Also wait when already disabled: a preceding disable can still be
    // pending. RWP covers SPI disable, not ICFGR writes or handler completion.
    if registers.wait_for_distributor_rwp() && !registers.is_irq_enable(intid) {
        registers.set_cfg(intid, trigger);
        if registers.get_cfg(intid) == trigger {
            if !was_enabled {
                return Ok(());
            }
            registers.set_irq_enable(intid, true);
            if registers.is_irq_enable(intid) {
                return Ok(());
            }
        }
    }
    // Do not restore on timeout or mismatch. This best-effort disable has no
    // completion wait: failure can leave partial state, including pending mask
    // writes. Returning an error makes no claim that hardware is masked.
    registers.set_irq_enable(intid, false);
    Err(kirq::IrqDescError::ConfigurationFailed {
        hwirq: intid.to_u32() as usize,
    })
}

/// Prepares SGI15 in allocator-ready, non-migrating startup context.
///
/// Registration is global and permanent; configuration and enablement are
/// local. Aff0 >= 16 is unsupported because this sender uses RS=0. MPIDR
/// affinity is immutable after CPU bring-up, so the notification can read it
/// directly without consulting a CPU map or allocating per-CPU storage.
pub(super) fn prepare_local_irq_work() -> Option<fn()> {
    let affinity = Affinity::current();
    if IRQ_WORK_SGI >= 16 || IRQ_WORK_SGI as usize == kbuild_config::IPI_IRQ || affinity.aff0 >= 16
    {
        warn!(
            "irq_work SGI15 preparation rejected: IPI_IRQ={}, Aff0={}",
            kbuild_config::IPI_IRQ,
            affinity.aff0
        );
        return None;
    }

    let desc = super::edge_irq_desc(IRQ_WORK_SGI as usize).with_flags(kirq::IrqFlags::PER_CPU);
    IRQ_WORK_HANDLER_REGISTERED.call_once(|| {
        // The reserved SGI mapping outlives its action and is shared by APs.
        // Keep mapping preparation separate from configuration and activation.
        if let Err(err) = kirq::try_map(desc) {
            warn!("irq_work mapping preparation failed: desc={desc:?}, error={err:?}");
            return false;
        }
        match kirq::try_register_percpu_startup(
            desc,
            Arc::new(|_| {
                kirq::irq_work::handle_irq();
                kirq::IrqEvent::HANDLED
            }),
        ) {
            Ok(true) => true,
            Ok(false) => {
                warn!("irq_work registration rejected: desc={desc:?}");
                false
            }
            Err(err) => {
                warn!("irq_work registration failed: desc={desc:?}, error={err:?}");
                false
            }
        }
    });
    if IRQ_WORK_HANDLER_REGISTERED.get() != Some(&true) {
        warn!("irq_work SGI15 handler registration failed");
        return None;
    }

    let gic = GIC.lock();
    #[cfg(feature = "nmi-hardware")]
    let mut gic = gic;
    let intid = IntId::sgi(IRQ_WORK_SGI);
    let cpu = gic.cpu_interface();
    cpu.set_irq_enable(intid, false);
    #[cfg(feature = "nmi-hardware")]
    if gic.supports_nmi_attributes()
        && gic
            .set_nmi_attribute(intid, NmiAttribute::Maskable)
            .is_err()
    {
        warn!("irq_work SGI15 could not be configured as an ordinary IRQ");
        return None;
    }
    cpu.set_cfg(intid, Trigger::Edge);
    cpu.set_priority(intid, karch::pmr::IRQ_THRESHOLD);
    drop(gic);
    if let Err(error) = kirq::try_enable(desc, true) {
        warn!("irq_work SGI15 enable failed: {error:?}");
        return None;
    }
    Some(notify_local_irq_work)
}

/// Issues the reserved ordinary SGI on the prepared current CPU, even in NMI.
///
/// Preparation validated Aff0 and the SGI number. Only register reads, integer
/// encoding and register writes occur here; the generic caller keeps this
/// execution on the prepared CPU. DSB publishes queued work before the SGI,
/// and ISB ensures issuance before returning. Do not use `send_sgi`: its trace
/// logging can re-enter locks held by the interrupted execution.
fn notify_local_irq_work() {
    let affinity = Affinity::current();
    let target = (u64::from(affinity.aff3) << 48)
        | (u64::from(affinity.aff2) << 32)
        | (u64::from(IRQ_WORK_SGI) << 24)
        | (u64::from(affinity.aff1) << 16)
        | (1u64 << affinity.aff0);
    aarch64_cpu::asm::barrier::dsb(aarch64_cpu::asm::barrier::ST);
    ICC_SGI1R_EL1.set(target);
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

#[cfg(unittest)]
pub(super) fn with_irq_work_gic_lock_for_test(action: impl FnOnce()) {
    let _guard = GIC.lock();
    action();
}

pub(super) fn init(gicd_base: memaddr::VirtAddr, gicr_base: memaddr::VirtAddr) {
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
    super::remember_standard_intid_limit(gic.typer_raw());
    GIC.init_once(SpinNoIrq::new(gic));
    let cpu = GIC.lock().cpu_interface();
    TRAP_OP.init_once(cpu.trap_operations());
}

pub(super) fn init_current_cpu() -> Result<(), GicInitError> {
    debug!("Initialize GICv3 CPU Interface...");
    let mut cpu = GIC.lock().cpu_interface();
    cpu.init_current_cpu()
        .map_err(|reason| GicInitError::CpuInterface { reason })?;
    cpu.set_eoi_mode(true);
    Ok(())
}

pub(super) fn set_trigger(interrupt_id: usize, edge: bool) {
    trace!("GICv3 set trigger: {interrupt_id} {edge}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(interrupt_id as u32) };
    let cfg = if edge { Trigger::Edge } else { Trigger::Level };
    GIC.lock().set_cfg(intid, cfg);
}

pub(super) fn enable(irq: usize, enabled: bool) {
    trace!("GICv3 set enable: {irq} {enabled}");
    // SAFETY: callers pass a hardware IRQ number understood by this GIC
    // instance; `IntId::raw` only wraps that validated numeric identifier.
    let intid = unsafe { IntId::raw(irq as u32) };
    let is_complete = {
        let mut gic = GIC.lock();
        if intid.is_private() {
            gic.set_irq_enable(intid, enabled);
            true
        } else {
            super::spi::set_enabled(&mut *gic, intid, enabled)
        }
    };
    if !is_complete {
        // Like Linux's mask path, this infallible hook diagnoses a bounded RWP
        // timeout and continues. Software admission remains disabled; neither
        // this return nor synchronize_irq proves physical-source quiescence.
        warn!("GICv3 SPI disable RWP timeout: hwirq={irq}");
    }
}

impl super::spi::Registers for Gic {
    fn set_enabled(&mut self, intid: IntId, enabled: bool) {
        Gic::set_irq_enable(self, intid, enabled);
    }

    fn wait_for_disable(&mut self) -> bool {
        Gic::wait_for_distributor_rwp(self).is_ok()
    }

    fn set_pending(&mut self, intid: IntId) {
        Gic::set_pending(self, intid, true);
    }
}

pub(super) fn retrigger_spi(hwirq: usize) {
    // SAFETY: the retained ordinary SPI binding was checked against the
    // discovered standard INTID limit before selection/publication.
    let intid = unsafe { IntId::raw(hwirq as u32) };
    super::spi::set_pending(&mut *GIC.lock(), intid);
}

pub(super) fn set_prio(irq: usize, priority: u8) {
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
pub(super) fn supports_hardware_nmi() -> bool {
    GIC.lock().supports_nmi_attributes()
}

/// Set or clear the GICv3.3 NMI attribute for `irq`.
///
/// Delegates to `arm-gic-driver`; returns `false` when the line cannot be
/// programmed (unsupported INTID, or the current CPU's redistributor frame
/// cannot be located).
#[cfg(feature = "nmi-hardware")]
pub(super) fn set_nmi_attr(irq: usize, nmi: bool) -> bool {
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
/// # Safety
/// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
pub(super) unsafe fn dispatch_irq_from_irqson() -> Option<kirq::Virq> {
    let Some((irq, cookie, is_nmi)) = ack_irq() else {
        // The exception came from an IRQ-on context. Even if the pending
        // interrupt retired before the acknowledge, restore the IRQ-on NMI
        // window before the common outer IRQ-exit tail, as Linux does after a
        // special INTID from IAR.
        unmask_nmis_from_irqson();
        return None;
    };

    if is_nmi {
        // SAFETY: ack_irq classified this unique claim as NMI. The caller
        // retains CPU/ordinary IRQ exclusion through its completion here.
        let pending = unsafe {
            kirq::PendingIrq::new(kirq::IrqRef::Domain(super::gic_domain(), irq), cookie)
        };
        kirq::generic_handle_nmi(pending);
    }

    // Linux does this after completing an IRQ-on NMI and before dispatching a
    // normal IRQ. It admits another NMI into the remaining outer IRQ context
    // without admitting normal IRQs.
    unmask_nmis_from_irqson();

    if is_nmi {
        None
    } else {
        // SAFETY: this is the unique normal claim; opening NMI nesting did
        // not admit ordinary IRQs or migration or leave hardirq context.
        let pending = unsafe {
            kirq::PendingIrq::new(kirq::IrqRef::Domain(super::gic_domain(), irq), cookie)
        };
        kirq::generic_handle_irq(pending)
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
pub(super) fn dispatch_irq_from_irqsoff() -> Option<(usize, usize)> {
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

pub(super) fn complete_irq(completion_cookie: usize) {
    // SAFETY: completion_cookie is the INTID returned by ack1() in
    // dispatch_irq on the same CPU; it is always a valid GICv3
    // interrupt identifier (special values already filtered out).
    let intid = unsafe { IntId::raw(completion_cookie as u32) };
    TRAP_OP.dir(intid);
}

pub(super) fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
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

unittest::path_mod_test!(delivery_tests, "../../ut-tests/gicv3_affinity_delivery.rs");
