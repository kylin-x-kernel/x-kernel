// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for user space.

use core::ops::{Deref, DerefMut};

use memaddr::VirtAddr;
#[cfg(feature = "fp-simd")]
use riscv::register::sstatus::FS;
use riscv::{
    interrupt::{
        Trap,
        supervisor::{Exception as E, Interrupt as I},
    },
    register::{scause, sstatus::Sstatus, stval},
};

pub use crate::userspace_common::{ExceptionKind, ReturnReason};
use crate::{ExceptionContext, GeneralRegisters, excp::PageFaultFlags};

/// Context to enter user space.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UserContext(ExceptionContext);

impl UserContext {
    /// Creates a new context with the given entry point, user stack pointer,
    /// and the argument.
    pub fn new(entry: usize, ustack_top: VirtAddr, arg0: usize) -> Self {
        let mut sstatus = Sstatus::from_bits(0);
        sstatus.set_spie(true); // enable interrupts
        sstatus.set_sum(true); // enable user memory access in supervisor mode
        #[cfg(feature = "fp-simd")]
        sstatus.set_fs(FS::Initial); // set the FPU to initial state

        Self(ExceptionContext {
            regs: GeneralRegisters {
                a0: arg0,
                sp: ustack_top.as_usize(),
                ..Default::default()
            },
            sepc: entry,
            sstatus,
        })
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

        let scause = scause::read();
        let ret = if let Ok(cause) = scause.cause().try_into::<I, E>() {
            let stval = stval::read();
            match cause {
                Trap::Interrupt(_) => {
                    dispatch_irq_trap!(IRQ, scause.bits());
                    ReturnReason::Interrupt
                }
                Trap::Exception(E::UserEnvCall) => {
                    self.sepc += 4;
                    ReturnReason::Syscall
                }
                Trap::Exception(E::LoadPageFault) => {
                    ReturnReason::PageFault(va!(stval), PageFaultFlags::READ | PageFaultFlags::USER)
                }
                Trap::Exception(E::StorePageFault) => ReturnReason::PageFault(
                    va!(stval),
                    PageFaultFlags::WRITE | PageFaultFlags::USER,
                ),
                Trap::Exception(E::InstructionPageFault) => ReturnReason::PageFault(
                    va!(stval),
                    PageFaultFlags::EXECUTE | PageFaultFlags::USER,
                ),
                Trap::Exception(e) => ReturnReason::Exception(ExceptionInfo { e, stval }),
            }
        } else {
            ReturnReason::Unknown
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
    pub e: E,
    /// The faulting address (from `stval`).
    pub stval: usize,
}

impl ExceptionInfo {
    /// Returns a generalized kind of this exception.
    pub fn kind(&self) -> ExceptionKind {
        match self.e {
            E::Breakpoint => ExceptionKind::Breakpoint,
            E::IllegalInstruction => ExceptionKind::IllegalInstruction,
            E::InstructionMisaligned | E::LoadMisaligned | E::StoreMisaligned => {
                ExceptionKind::Misaligned
            }
            _ => ExceptionKind::Other,
        }
    }
}
