// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for user space.

use core::ops::{Deref, DerefMut};

use aarch64_cpu::registers::{ESR_EL1, FAR_EL1, Readable};
use memaddr::VirtAddr;
use tock_registers::LocalRegisterCopy;

use super::excp::{ArchTrap, check_page_fault};
// Use crate::ExceptionContext if exposed, or stick to TrapFrame alias
// Since I want to rename things, I should try to use ExceptionContext
use crate::aarch64::ExceptionContext;
use crate::excp::PageFaultFlags;
pub use crate::userspace_common::{ExceptionKind, ReturnReason};

/// Context to enter user space.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct UserContext {
    tf: ExceptionContext,
    /// Stack Pointer (SP_EL0).
    pub sp: u64,
    /// Software Thread ID Register (TPIDR_EL0).
    pub tpidr: u64,
}

impl UserContext {
    const PAD_MAGIC: u64 = 0x1234_5678_9abc_def0;

    /// Creates a new context with the given entry point, user stack pointer,
    /// and the argument.
    pub fn new(entry: usize, ustack_top: VirtAddr, arg0: usize) -> Self {
        use aarch64_cpu::registers::SPSR_EL1;
        let mut regs = [0; 31];
        regs[0] = arg0 as _;
        Self {
            tf: ExceptionContext {
                x: regs,
                elr: entry as _,
                spsr: (SPSR_EL1::M::EL0t
                    + SPSR_EL1::D::Masked
                    + SPSR_EL1::A::Masked
                    + SPSR_EL1::I::Unmasked
                    + SPSR_EL1::F::Masked)
                    .value,
                __pad: Self::PAD_MAGIC,
            },
            sp: ustack_top.as_usize() as _,
            tpidr: 0,
        }
    }

    /// Gets the stack pointer.
    pub const fn sp(&self) -> usize {
        self.sp as _
    }

    /// Sets the stack pointer.
    pub const fn set_sp(&mut self, sp: usize) {
        self.sp = sp as _;
    }

    /// Gets the TLS area.
    pub const fn tls(&self) -> usize {
        self.tpidr as _
    }

    /// Sets the TLS area.
    pub const fn set_tls(&mut self, tls: usize) {
        self.tpidr = tls as _;
    }

    /// Enters user space.
    ///
    /// It restores the user registers and jumps to the user entry point
    /// (saved in `elr`).
    ///
    /// This function returns when an exception or syscall occurs.
    pub fn run(&mut self) -> ReturnReason {
        unsafe extern "C" {
            unsafe fn enter_user(uctx: &mut UserContext) -> ArchTrap;
        }

        crate::instrs::disable_local(); // updated module reference from asm -> instrs
        let trap_kind = unsafe { enter_user(self) };

        let ret = match trap_kind {
            ArchTrap::Irq => {
                dispatch_irq_trap!(IRQ, 0);
                ReturnReason::Interrupt
            }
            ArchTrap::Fiq | ArchTrap::SError => ReturnReason::Unknown,
            ArchTrap::Synchronous => {
                let esr = ESR_EL1.extract();
                let far = FAR_EL1.get() as usize;

                let iss = esr.read(ESR_EL1::ISS);

                match esr.read_as_enum(ESR_EL1::EC) {
                    Some(ESR_EL1::EC::Value::SVC64) => ReturnReason::Syscall,
                    Some(ESR_EL1::EC::Value::InstrAbortLowerEL) if check_page_fault(iss) => {
                        ReturnReason::PageFault(
                            va!(far),
                            PageFaultFlags::EXECUTE | PageFaultFlags::USER,
                        )
                    }
                    Some(ESR_EL1::EC::Value::DataAbortLowerEL) if check_page_fault(iss) => {
                        let wnr = (iss & (1 << 6)) != 0; // WnR: Write not Read
                        let cm = (iss & (1 << 8)) != 0; // CM: Cache maintenance
                        ReturnReason::PageFault(
                            va!(far),
                            if wnr & !cm {
                                PageFaultFlags::WRITE
                            } else {
                                PageFaultFlags::READ
                            } | PageFaultFlags::USER,
                        )
                    }
                    _ => ReturnReason::Exception(ExceptionInfo { esr, far }),
                }
            }
        };

        crate::instrs::enable_local();
        ret
    }
}

impl Deref for UserContext {
    type Target = ExceptionContext;

    fn deref(&self) -> &Self::Target {
        &self.tf
    }
}

impl DerefMut for UserContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tf
    }
}

/// Information about an exception that occurred in user space.
#[derive(Debug, Clone, Copy)]
pub struct ExceptionInfo {
    /// Exception Syndrome Register
    pub esr: LocalRegisterCopy<u64, ESR_EL1::Register>,
    /// Fault Address Register
    pub far: usize,
}

impl ExceptionInfo {
    /// Returns a generalized kind of this exception.
    pub fn kind(&self) -> ExceptionKind {
        match self.esr.read_as_enum(ESR_EL1::EC) {
            Some(ESR_EL1::EC::Value::BreakpointLowerEL) => ExceptionKind::Breakpoint,
            Some(ESR_EL1::EC::Value::IllegalExecutionState) => ExceptionKind::IllegalInstruction,
            Some(ESR_EL1::EC::Value::PCAlignmentFault)
            | Some(ESR_EL1::EC::Value::SPAlignmentFault) => ExceptionKind::Misaligned,
            _ => ExceptionKind::Other,
        }
    }
}
