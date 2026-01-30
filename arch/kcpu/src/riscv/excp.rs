// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V exception and IRQ dispatching.

#[cfg(feature = "fp-simd")]
use riscv::register::sstatus;
use riscv::{
    interrupt::{
        Trap,
        supervisor::{Exception as E, Interrupt as I},
    },
    register::{scause, stval},
};

use super::ExceptionContext;
use crate::excp::PageFaultFlags;

core::arch::global_asm!(
    include_asm_macros!(),
    include_str!("excp.S"),
    trapframe_size = const core::mem::size_of::<ExceptionContext>(),
);

/// Advances the PC after a breakpoint exception.
fn dispatch_irq_breakpoint(sepc: &mut usize) {
    debug!("Exception(Breakpoint) @ {sepc:#x} ");
    *sepc += 2
}

/// Dispatches a page fault and panics if unhandled.
fn dispatch_irq_page_fault(tf: &mut ExceptionContext, access_flags: PageFaultFlags) {
    let vaddr = va!(stval::read());
    if dispatch_irq_trap!(PAGE_FAULT, vaddr, access_flags) {
        return;
    }
    #[cfg(feature = "uspace")]
    if tf.fixup_exception() {
        return;
    }
    core::hint::cold_path();
    panic!(
        "Undispatch_irqd Supervisor Page Fault @ {:#x}, fault_vaddr={:#x} ({:?}):\n{:#x?}\n{}",
        tf.sepc,
        vaddr,
        access_flags,
        tf,
        tf.backtrace()
    );
}

/// Architecture-specific trap entry point.
#[unsafe(no_mangle)]
fn riscv_trap_handler(tf: &mut ExceptionContext) {
    let _tf_guard = crate::ExceptionContextGuard::new(tf);
    let scause = scause::read();
    if let Ok(cause) = scause.cause().try_into::<I, E>() {
        match cause {
            Trap::Exception(E::LoadPageFault) => dispatch_irq_page_fault(tf, PageFaultFlags::READ),
            Trap::Exception(E::StorePageFault) => {
                dispatch_irq_page_fault(tf, PageFaultFlags::WRITE)
            }
            Trap::Exception(E::InstructionPageFault) => {
                dispatch_irq_page_fault(tf, PageFaultFlags::EXECUTE)
            }
            Trap::Exception(E::Breakpoint) => dispatch_irq_breakpoint(&mut tf.sepc),
            Trap::Interrupt(_) => {
                dispatch_irq_trap!(IRQ, scause.bits());
            }
            _ => {
                panic!(
                    "Undispatch_irqd trap {:?} @ {:#x}, stval={:#x}:\n{:#x?}\n{}",
                    cause,
                    tf.sepc,
                    stval::read(),
                    tf,
                    tf.backtrace()
                );
            }
        }
    } else {
        panic!(
            "Unknown trap {:#x?} @ {:#x}:\n{:#x?}\n{}",
            scause.cause(),
            tf.sepc,
            tf,
            tf.backtrace()
        );
    }

    // Update tf.sstatus to preserve current hardware FS state
    // This replaces the assembly-level FS handling workaround
    #[cfg(feature = "fp-simd")]
    tf.sstatus.set_fs(sstatus::read().fs());
}
