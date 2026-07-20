// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// The import layout and user-context definition in this file follow common
// Rust module organization and standard AArch64 user-space context conventions.
// Similar structure across kernels is expected for clarity and ABI alignment,
// and should not be interpreted as literal duplication of project-specific code.

//! Structures and functions for user space.

use aarch64_cpu::registers::{ESR_EL1, FAR_EL1, Readable};
use kerrno::LinuxError;
use memaddr::VirtAddr;
use tock_registers::LocalRegisterCopy;

use super::excp::{ArchTrap, check_page_fault};
// Use crate::ExceptionContext if exposed, or stick to TrapFrame alias
// Since I want to rename things, I should try to use ExceptionContext
use crate::aarch64::ExceptionContext;
use crate::excp::PageFaultFlags;
pub use crate::userspace_common::{ExceptionKind, ReturnReason};

#[unsafe(no_mangle)]
extern "C" fn kcpu_prepare_enter_user_irq() {
    karch::prepare_enter_user_irq();
}

/// Context to enter user space.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct UserContext {
    tf: ExceptionContext,
    /// Stack Pointer (SP_EL0).
    pub sp: u64,
    /// Software Thread ID Register (TPIDR_EL0).
    pub tpidr: u64,
    /// Snapshot of x0 saved before dispatching a syscall, so that
    /// SA_RESTART can restore argument 0 after the return value
    /// overwrites it.
    saved_syscall_arg0: u64,
    /// Whether the most recent user trap was a syscall (`svc`).
    ///
    /// Software-only; assembly does not touch this field. It prevents treating
    /// a coincidental user `x0` value as a Linux restart code after IRQ/fault.
    from_syscall: bool,
}

/// User-space state saved outside the ABI `mcontext_t` signal payload.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UserRestorableContext {
    tpidr: u64,
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
            saved_syscall_arg0: 0,
            from_syscall: false,
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

    /// Saves user-restorable state that is not carried by signal `mcontext_t`.
    pub fn save_user_restorable(&self) -> UserRestorableContext {
        UserRestorableContext { tpidr: self.tpidr }
    }

    /// Restores user state that is not carried by signal `mcontext_t`.
    pub fn restore_user_restorable(&mut self, saved: UserRestorableContext) {
        self.tpidr = saved.tpidr;
        self.saved_syscall_arg0 = 0;
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

        karch::disable_local_irq(); // updated module reference from asm -> instrs
        // SAFETY: `enter_user` is an assembly stub that restores user registers from
        // `UserContext` and executes `eret`. `UserContext` fields are set up by `new()`
        // with valid EL0t SPSR and user entry point.
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
                    Some(ESR_EL1::EC::Value::TrappedWFIorWFE) => {
                        let next_ip = self.ip() + 4;
                        self.set_ip(next_ip);
                        ReturnReason::Interrupt
                    }
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

        // Only syscall traps may interpret x0 as a Linux restart code.
        self.set_from_syscall(matches!(ret, ReturnReason::Syscall));

        karch::enable_local_irq();
        ret
    }
}

impl UserContext {
    /// Returns whether the most recent user trap was a syscall entry.
    pub const fn is_from_syscall(&self) -> bool {
        self.from_syscall
    }

    /// Records whether the trap that just left user space was a syscall.
    pub(crate) fn set_from_syscall(&mut self, from_syscall: bool) {
        self.from_syscall = from_syscall;
    }

    /// Snapshot x0 before entering the syscall dispatch so that it can
    /// be restored later by [`rollback_syscall`].
    pub fn save_syscall_args(&mut self) {
        self.saved_syscall_arg0 = self.tf.x[0];
    }

    /// Rewind the program counter so that the SVC instruction that entered
    /// the kernel is re-executed when we return to userspace.  Used by the
    /// SA_RESTART machinery to transparently restart an interrupted syscall.
    pub fn rollback_syscall(&mut self) {
        if !self.is_from_syscall() {
            return;
        }
        // On AArch64, ELR holds the address of the instruction *after* SVC.
        self.tf.elr = self.tf.elr.wrapping_sub(4);
        // Restore the original syscall argument; x0 was overwritten with
        // the return value (e.g. ERESTARTSYS).
        self.tf.x[0] = self.saved_syscall_arg0;
    }

    /// Replace the syscall number and rewind PC so that the new syscall
    /// is executed instead of the original one (used by ERESTART_RESTARTBLOCK).
    pub fn restart_with_syscall(&mut self, sysno: usize) {
        if !self.is_from_syscall() {
            return;
        }
        self.rollback_syscall();
        self.set_sysno(sysno);
    }

    /// If the last syscall set a Linux restart error as its return value,
    /// returns that error; otherwise `None`.
    pub fn syscall_restart_error(&self) -> Option<LinuxError> {
        if !self.is_from_syscall() {
            return None;
        }
        let retval = self.retval() as isize;
        [
            LinuxError::ERESTARTSYS,
            LinuxError::ERESTARTNOINTR,
            LinuxError::ERESTARTNOHAND,
            LinuxError::ERESTART_RESTARTBLOCK,
        ]
        .into_iter()
        .find(|err| retval == -(err.into_raw() as isize))
    }
}

impl_user_context_deref!(ExceptionContext, tf);

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
        exception_kind_from_ec(self.esr.read_as_enum(ESR_EL1::EC))
    }
}

fn exception_kind_from_ec(ec: Option<ESR_EL1::EC::Value>) -> ExceptionKind {
    use ESR_EL1::EC::Value;

    match ec {
        Some(
            Value::BreakpointLowerEL
            | Value::BreakpointCurrentEL
            | Value::SoftwareStepLowerEL
            | Value::SoftwareStepCurrentEL
            | Value::WatchpointLowerEL
            | Value::WatchpointCurrentEL
            | Value::Bkpt32
            | Value::Brk64,
        ) => ExceptionKind::Breakpoint,
        Some(Value::PCAlignmentFault | Value::SPAlignmentFault) => ExceptionKind::Misaligned,
        Some(
            Value::Unknown
            | Value::TrappedMCRorMRC
            | Value::TrappedMCRRorMRRC
            | Value::TrappedMCRorMRC2
            | Value::TrappedLDCorSTC
            | Value::TrappedFP
            | Value::TrappedMRRC
            | Value::BranchTarget
            | Value::IllegalExecutionState
            | Value::TrappedMsrMrs
            | Value::TrappedSve
            | Value::PointerAuth,
        ) => ExceptionKind::IllegalInstruction,
        _ => ExceptionKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use aarch64_cpu::registers::ESR_EL1;

    use super::{ExceptionKind, exception_kind_from_ec};

    #[test]
    fn classifies_instruction_traps_as_illegal_instruction() {
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedFP)),
            ExceptionKind::IllegalInstruction
        );
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedMsrMrs)),
            ExceptionKind::IllegalInstruction
        );
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedSve)),
            ExceptionKind::IllegalInstruction
        );
    }

    #[test]
    fn keeps_breakpoint_and_alignment_classes_distinct() {
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::Brk64)),
            ExceptionKind::Breakpoint
        );
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::PCAlignmentFault)),
            ExceptionKind::Misaligned
        );
        assert_eq!(
            exception_kind_from_ec(Some(ESR_EL1::EC::Value::TrappedWFIorWFE)),
            ExceptionKind::Other
        );
    }
}
