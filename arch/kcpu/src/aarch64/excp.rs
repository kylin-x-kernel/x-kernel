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
    // GICv3-only: mrs/msr ICC_PMR_EL1. GICv2 has no sysreg interface —
    // enabling pmr on GICv2 will cause undefined-instruction exceptions.
    PMR_SYSREG = const if cfg!(feature = "pmr") { 1u64 } else { 0u64 },
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

/// Architecture-specific trap entry point.
#[unsafe(no_mangle)]
fn dispatch_exception(tf: &mut ExceptionContext, kind: ArchTrap, source: ArchTrapOrigin) {
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
            #[cfg(feature = "pmr")]
            if tf.spsr & (1 << 7) != 0
                || (karch::pmr::is_ready() && tf.pmr as u8 <= karch::pmr::NMI_ONLY)
            {
                // PMR <= NMI_ONLY means only interrupts with priority below
                // that threshold could have been delivered — i.e. pseudo‑NMI
                // sources.  This relies on the invariant that NMI sources are
                // programmed at priority 0 while every other interrupt stays
                // at or above IRQ_THRESHOLD, enforced by the GIC init
                // defaults and irq_driver::gic::set_prio.
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
