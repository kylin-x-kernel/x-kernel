// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for user space.

use core::ops::{Deref, DerefMut};

use loongArch64::register::{
    badi, badv,
    estat::{self, Exception, Trap},
};
use memaddr::VirtAddr;

pub use crate::userspace_common::{ExceptionKind, ReturnReason};
use crate::{ExceptionContext, excp::PageFaultFlags};

/// Context to enter user space.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UserContext(ExceptionContext);

impl UserContext {
    /// Creates a new context with the given entry point, user stack pointer,
    /// and the argument.
    pub fn new(entry: usize, ustack_top: VirtAddr, arg0: usize) -> Self {
        let mut trap_frame = ExceptionContext::default();
        const PPLV_UMODE: usize = 0b11;
        const PIE: usize = 1 << 2;
        trap_frame.regs.sp = ustack_top.as_usize();
        trap_frame.era = entry;
        trap_frame.prmd = PPLV_UMODE | PIE;
        trap_frame.regs.a0 = arg0;
        Self(trap_frame)
    }

    /// Enter user space.
    ///
    /// It restores the user registers and jumps to the user entry point
    /// (saved in `sepc`).
    ///
    /// This function returns when an exception or syscall occurs.
    pub fn run(&mut self) -> ReturnReason {
        extern "C" {
            fn enter_user(uctx: &mut UserContext);
        }

        crate::instrs::disable_local();
        unsafe { enter_user(self) };

        let estat = estat::read();
        let badv = badv::read().vaddr();
        let badi = badi::read().inst();

        let ret = match estat.cause() {
            Trap::Interrupt(_) => {
                let interrupt_id: usize = estat.is().trailing_zeros() as usize;
                dispatch_irq_trap!(IRQ, interrupt_id);
                ReturnReason::Interrupt
            }
            Trap::Exception(Exception::Syscall) => {
                self.era += 4;
                ReturnReason::Syscall
            }
            Trap::Exception(Exception::LoadPageFault)
            | Trap::Exception(Exception::PageNonReadableFault) => {
                ReturnReason::PageFault(va!(badv), PageFaultFlags::READ | PageFaultFlags::USER)
            }
            Trap::Exception(Exception::StorePageFault)
            | Trap::Exception(Exception::PageModifyFault) => {
                ReturnReason::PageFault(va!(badv), PageFaultFlags::WRITE | PageFaultFlags::USER)
            }
            Trap::Exception(Exception::FetchPageFault)
            | Trap::Exception(Exception::PageNonExecutableFault) => {
                ReturnReason::PageFault(va!(badv), PageFaultFlags::EXECUTE | PageFaultFlags::USER)
            }
            Trap::Exception(e) => ReturnReason::Exception(ExceptionInfo { e, badv, badi }),
            _ => ReturnReason::Unknown,
        };

        crate::instrs::enable_local();
        ret
    }
}

impl Deref for UserContext {
    type Target = ExceptionContext;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for UserContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Information about an exception that occurred in user space.
#[derive(Debug, Clone, Copy)]
pub struct ExceptionInfo {
    /// The raw exception.
    pub e: Exception,
    /// The faulting address (from `badv`).
    pub badv: usize,
    /// The instruction causing the fault (from `badi`).
    pub badi: u32,
}

impl ExceptionInfo {
    /// Returns a generalized kind of this exception.
    pub fn kind(&self) -> ExceptionKind {
        match self.e {
            Exception::Breakpoint => ExceptionKind::Breakpoint,
            Exception::InstructionNotExist | Exception::InstructionPrivilegeIllegal => {
                ExceptionKind::IllegalInstruction
            }
            Exception::AddressNotAligned => ExceptionKind::Misaligned,
            _ => ExceptionKind::Other,
        }
    }
}
