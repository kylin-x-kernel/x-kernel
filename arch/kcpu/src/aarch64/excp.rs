// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 exception and IRQ dispatching.

use aarch64_cpu::registers::{ESR_EL1, FAR_EL1};
use tock_registers::interfaces::Readable;

use super::ExceptionContext;
use crate::excp::PageFaultFlags;

#[repr(u8)]
#[derive(Debug)]
pub(super) enum ArchTrap {
    Synchronous = 0,
    Irq         = 1,
    Fiq         = 2,
    SError      = 3,
}

#[repr(u8)]
#[derive(Debug)]
enum ArchTrapOrigin {
    CurrentSpEl0 = 0,
    CurrentSpElx = 1,
    LowerAArch64 = 2,
    LowerAArch32 = 3,
}

core::arch::global_asm!(
    include_str!("excp.S"),
    trapframe_size = const core::mem::size_of::<ExceptionContext>(),
    TRAP_KIND_SYNC = const ArchTrap::Synchronous as u8,
    TRAP_KIND_IRQ = const ArchTrap::Irq as u8,
    TRAP_KIND_FIQ = const ArchTrap::Fiq as u8,
    TRAP_KIND_SERROR = const ArchTrap::SError as u8,
    TRAP_SRC_CURR_EL0 = const ArchTrapOrigin::CurrentSpEl0 as u8,
    TRAP_SRC_CURR_ELX = const ArchTrapOrigin::CurrentSpElx as u8,
    TRAP_SRC_LOWER_AARCH64 = const ArchTrapOrigin::LowerAArch64 as u8,
    TRAP_SRC_LOWER_AARCH32 = const ArchTrapOrigin::LowerAArch32 as u8,
);

#[inline(always)]
/// Returns true if the ISS indicates a translation or permission fault.
pub(super) fn check_page_fault(iss: u64) -> bool {
    // Only dispatch_irq Translation fault and Permission fault
    matches!(iss & 0b111100, 0b0100 | 0b1100)
}

/// Dispatches a page fault and panics if unhandled.
fn handle_page_fault(tf: &mut ExceptionContext, access_flags: PageFaultFlags) {
    let vaddr = va!(FAR_EL1.get() as usize);
    if dispatch_irq_trap!(PAGE_FAULT, vaddr, access_flags) {
        return;
    }
    if tf.fixup_exception() {
        return;
    }
    core::hint::cold_path();
    panic!(
        "Unhandled EL1 Page Fault @ {:#x}, fault_vaddr={:#x}, ESR={:#x} ({:?}):\n{:#x?}\n{}",
        tf.elr,
        vaddr,
        ESR_EL1.get(),
        access_flags,
        tf,
        tf.backtrace()
    );
}

/// Pseudo-NMI exception entry/exit handling, gated on the mechanism readiness
/// flag that the platform derives from `detect_mode()`, so a degraded build
/// (no active mechanism) behaves exactly like a plain-IRQ kernel and never
/// touches the NMI-only registers.
///
/// Pseudo-NMI: saves the entry `ICC_PMR_EL1` value into the trapframe
/// (where [`use_nmi_path`] classifies the interrupt) and opens the mask,
/// then restores PMR before returning to the assembly epilogue.
///
/// Hardware NMI needs no entry/exit handling here: exception entry
/// (SPINTMASK=0) sets `PSTATE.ALLINT=1` for the duration of the handler, the
/// GIC IRQ dispatch path opens the ALLINT window for a normal IRQ so a
/// Superpriority NMI can preempt it, and `ERET` restores the interrupted
/// context's exact PSTATE from `SPSR_EL1` (which exception entry saved
/// *before* setting ALLINT — the saved SPSR must not be modified).
#[cfg(feature = "nmi-pseudo")]
struct NmiExceptionGuard {
    saved_pmr: u8,
    pmr_active: bool,
}

#[cfg(feature = "nmi-pseudo")]
impl NmiExceptionGuard {
    fn new(tf: &mut ExceptionContext) -> Self {
        let (saved_pmr, pmr_active) = if karch::pmr::is_ready() {
            // Save the entry PMR so `use_nmi_path` can classify the
            // interrupt and the exit path can restore it, then open the
            // mask (the assembly prologue used to do this).  `is_ready()` is
            // only set on GICv3, so a degraded build skips this entirely.
            let saved = karch::pmr::read();
            tf.pmr = saved as u64;
            karch::pmr::write(karch::pmr::ALL);
            (saved, true)
        } else {
            (0, false)
        };
        Self {
            saved_pmr,
            pmr_active,
        }
    }
}

#[cfg(feature = "nmi-pseudo")]
impl Drop for NmiExceptionGuard {
    fn drop(&mut self) {
        if self.pmr_active {
            // Mask IRQs while restoring PMR (the assembly epilogue used to
            // do this); `eret` restores the interrupted DAIF.I afterwards.
            // SAFETY: `msr daifset` only masks IRQs; the PMR value was saved
            // at entry on this CPU.
            unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack)) };
            karch::pmr::write(self.saved_pmr);
        }
    }
}

/// Architecture-specific trap entry point.
///
/// Pseudo-NMI PMR save/restore is performed here, gated on the runtime NMI
/// mode so a degraded build (no active mechanism) behaves exactly like a
/// plain-IRQ kernel and never touches the NMI-only registers.  Hardware-NMI
/// ALLINT handling lives in the GIC dispatch paths
/// (`dispatch_irq_from_irqson` / `dispatch_irq_from_irqsoff`), not in the
/// exception entry.
#[unsafe(no_mangle)]
fn dispatch_exception(tf: &mut ExceptionContext, kind: ArchTrap, source: ArchTrapOrigin) {
    #[cfg(feature = "nmi-pseudo")]
    let _nmi_guard = NmiExceptionGuard::new(tf);
    let _tf_guard = crate::ExceptionContextGuard::new(tf);
    if matches!(
        source,
        ArchTrapOrigin::CurrentSpEl0 | ArchTrapOrigin::LowerAArch64 | ArchTrapOrigin::LowerAArch32
    ) {
        panic!(
            "Invalid exception {:?} from {:?}:\n{:#x?}",
            kind, source, tf
        );
    }
    match kind {
        ArchTrap::Fiq | ArchTrap::SError => {
            panic!("Unhandled exception {:?}:\n{:#x?}", kind, tf);
        }
        ArchTrap::Irq => {
            #[cfg(feature = "nmi")]
            if use_nmi_path(tf) {
                dispatch_irq_trap!(NMI, 0);
                return;
            }
            dispatch_irq_trap!(IRQ, 0);
        }
        ArchTrap::Synchronous => {
            let esr = ESR_EL1.extract();
            let iss = esr.read(ESR_EL1::ISS);
            match esr.read_as_enum(ESR_EL1::EC) {
                Some(ESR_EL1::EC::Value::InstrAbortCurrentEL) if check_page_fault(iss) => {
                    handle_page_fault(tf, PageFaultFlags::EXECUTE);
                }
                Some(ESR_EL1::EC::Value::DataAbortCurrentEL) if check_page_fault(iss) => {
                    let wnr = (iss & (1 << 6)) != 0; // WnR: Write not Read
                    let cm = (iss & (1 << 8)) != 0; // CM: Cache maintenance
                    handle_page_fault(
                        tf,
                        if wnr & !cm {
                            PageFaultFlags::WRITE
                        } else {
                            PageFaultFlags::READ
                        },
                    );
                }
                Some(ESR_EL1::EC::Value::Brk64) => {
                    debug!("BRK #{:#x} @ {:#x} ", iss, tf.elr);
                    tf.elr += 4;
                }
                e => {
                    if tf.fixup_exception() {
                        return;
                    }
                    let vaddr = va!(FAR_EL1.get() as usize);
                    panic!(
                        "Unhandled synchronous exception {:?} @ {:#x}: ESR={:#x} (EC {:#08b}, \
                         FAR: {:#x} ISS {:#x})\n{}",
                        e,
                        tf.elr,
                        esr.get(),
                        esr.read(ESR_EL1::EC),
                        vaddr,
                        iss,
                        tf.backtrace()
                    );
                }
            }
        }
    }
}

/// Whether the IRQ exception just taken must be dispatched through the
/// lock‑free NMI path.
///
/// Routing is decided by the interrupted context's mask state, not by the
/// type of interrupt that fired:
///
/// - `SPSR.I == 1`: the IRQ exception bypassed `DAIF.I`.  With hardware NMI
///   (FEAT_NMI) only an IRQ with Superpriority can do that, so the NMI path
///   is unambiguous; with pseudo‑NMI the PMR window is the only way an IRQ
///   arrives while `DAIF.I` is set.
/// - `SPSR.I == 0`: the context had IRQs enabled.  A hardware NMI that
///   interrupted such a context still runs on the normal IRQ path (the GIC
///   backend is responsible for acknowledging it — IAR1 returns a special
///   INTID for pending NMI-attribute interrupts, and the real ack happens
///   via `ICC_NMIAR1_EL1`); returning `false` here means "the context had
///   IRQs enabled", not "this is not an NMI".
#[cfg(feature = "nmi")]
fn use_nmi_path(tf: &ExceptionContext) -> bool {
    // Hardware NMI (FEAT_NMI): an IRQ exception taken while the interrupted
    // context had IRQs masked must have bypassed DAIF.I.  Gated on the
    // mechanism readiness flag so a degraded build never classifies an IRQ
    // as an NMI.
    #[cfg(feature = "nmi-hardware")]
    if karch::allint_active() && tf.spsr & (1 << 7) != 0 {
        return true;
    }

    // Pseudo-NMI: PMR lowered to NMI_ONLY lets only priority-0 (NMI)
    // sources through, so the taken interrupt must be one.  `pmr::is_ready`
    // is only set on GICv3, so a degraded build never takes this path.
    #[cfg(feature = "nmi-pseudo")]
    if karch::pmr::is_ready() && tf.pmr as u8 <= karch::pmr::NMI_ONLY {
        return true;
    }
    false
}
