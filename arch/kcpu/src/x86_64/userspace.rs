// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// The import layout and user-context definition in this file follow common
// Rust module organization and standard X86-64 user-space context conventions.
// Similar structure across kernels is expected for clarity and ABI alignment,
// and should not be interpreted as literal duplication of project-specific

//! Structures and functions for user space.

use kerrno::LinuxError;
use memaddr::VirtAddr;
use x86_64::{
    registers::{
        control::Cr2,
        model_specific::{Efer, EferFlags, KernelGsBase, LStar, SFMask, Star},
        rflags::RFlags,
    },
    structures::idt::ExceptionVector,
};

use super::{
    TrapFrame,
    excp::{IRQ_VECTOR_END, IRQ_VECTOR_START, LEGACY_SYSCALL_VECTOR, err_code_to_flags},
    gdt,
};
pub use crate::userspace_common::{ExceptionKind, ReturnReason};

/// Context to enter user space.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UserContext {
    tf: TrapFrame,
    /// FS Segment Base
    pub fs_base: u64,
    /// GS Segment Base
    pub gs_base: u64,
}

/// Private frame used by `enter_user` while running user space.
///
/// The x86_64 entry assembly uses `tf` as a temporary trap stack: TSS.rsp0
/// points to the end of `tf`, and hardware trap frames grow down into it.
/// `kernel_rsp` is kept immediately before `tf` so the return path can recover
/// the Rust caller's kernel stack without storing that private value in
/// [`UserContext`] or any user-visible signal frame.
#[repr(C, align(16))]
struct EnterUserFrame {
    kernel_rsp: u64,
    tf: TrapFrame,
}

/// User-space state saved outside the ABI `mcontext_t` signal payload.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct UserRestorableContext {
    fs_base: u64,
    gs_base: u64,
}

pub(super) const KERNEL_RSP_TO_TRAPFRAME_OFFSET: usize =
    core::mem::offset_of!(EnterUserFrame, tf) - core::mem::offset_of!(EnterUserFrame, kernel_rsp);

const _: () = assert!(KERNEL_RSP_TO_TRAPFRAME_OFFSET == core::mem::size_of::<u64>());

impl UserContext {
    /// Creates a new context with the given entry point, user stack pointer,
    /// and the argument.
    pub fn new(entry: usize, ustack_top: VirtAddr, arg0: usize) -> Self {
        use x86_64::registers::rflags::RFlags;
        Self {
            tf: TrapFrame {
                rdi: arg0 as _,
                rip: entry as _,
                cs: gdt::UCODE64.0 as _,
                rflags: RFlags::INTERRUPT_FLAG.bits(), // IOPL = 0, IF = 1
                rsp: ustack_top.as_usize() as _,
                ss: gdt::UDATA.0 as _,
                orig_rax: u64::MAX,
                ..Default::default()
            },
            fs_base: 0,
            gs_base: 0,
        }
    }

    /// Gets the TLS area.
    pub const fn tls(&self) -> usize {
        self.fs_base as _
    }

    /// Saves user-restorable state that is not carried by signal `mcontext_t`.
    pub fn save_user_restorable(&self) -> UserRestorableContext {
        UserRestorableContext {
            fs_base: self.fs_base,
            gs_base: self.gs_base,
        }
    }

    /// Restores user state that is not carried by signal `mcontext_t`.
    pub fn restore_user_restorable(&mut self, saved: UserRestorableContext) {
        self.fs_base = saved.fs_base;
        self.gs_base = saved.gs_base;
        self.ss = gdt::UDATA.0 as _;
        self.orig_rax = u64::MAX;
    }

    /// Sets the TLS area.
    pub const fn set_tls(&mut self, tls_area: usize) {
        self.fs_base = tls_area as _;
    }

    /// Enters user space.
    ///
    /// It restores the user registers and jumps to the user entry point
    /// (saved in `rip`).
    ///
    /// This function returns when an exception or syscall occurs.
    pub fn run(&mut self) -> ReturnReason {
        unsafe extern "C" {
            unsafe fn enter_user(frame: &mut EnterUserFrame);
        }

        assert_eq!(self.cs, gdt::UCODE64.0 as _);
        assert_eq!(self.ss, gdt::UDATA.0 as _);

        karch::disable_local_irq();

        let kernel_fs_base = karch::read_thread_pointer();
        // SAFETY: Setting FS base to the user value; the kernel value is saved and restored after `enter_user` returns.
        unsafe { karch::write_thread_pointer(self.fs_base as _) };
        KernelGsBase::write(x86_64::VirtAddr::new_truncate(self.gs_base));

        let mut frame = EnterUserFrame {
            kernel_rsp: 0,
            tf: self.tf,
        };

        // SAFETY: `enter_user` switches to user mode using the private
        // `EnterUserFrame` layout above. `self.tf` was initialized by `new()` or
        // restored from validated signal state with user segment selectors.
        unsafe { enter_user(&mut frame) };
        self.tf = frame.tf;

        self.gs_base = KernelGsBase::read().as_u64();
        self.fs_base = karch::read_thread_pointer() as _;
        // SAFETY: Restoring the kernel FS base that was saved before entering user mode.
        unsafe { karch::write_thread_pointer(kernel_fs_base) };

        let cr2 = Cr2::read().unwrap().as_u64() as usize;
        let vector = self.vector as u8;

        const PAGE_FAULT_VECTOR: u8 = ExceptionVector::Page as u8;

        let ret = match vector {
            PAGE_FAULT_VECTOR if let Ok(flags) = err_code_to_flags(self.error_code) => {
                ReturnReason::PageFault(va!(cr2), flags)
            }
            LEGACY_SYSCALL_VECTOR => ReturnReason::Syscall,
            IRQ_VECTOR_START..=IRQ_VECTOR_END => {
                dispatch_irq_trap!(IRQ, vector as _);
                ReturnReason::Interrupt
            }
            _ => ReturnReason::Exception(ExceptionInfo {
                vector,
                error_code: self.error_code,
                cr2,
            }),
        };

        karch::enable_local_irq();
        ret
    }

    /// Returns the saved Linux restart error when this context stopped after a syscall.
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

impl_user_context_deref!(TrapFrame, tf);

/// Information about an exception that occurred in user space.
#[derive(Debug, Clone, Copy)]
pub struct ExceptionInfo {
    /// The exception vector.
    pub vector: u8,
    /// The error code.
    pub error_code: u64,
    /// The faulting virtual address (if applicable).
    pub cr2: usize,
}

impl ExceptionInfo {
    /// Returns a generalized kind of this exception.
    pub fn kind(&self) -> ExceptionKind {
        match ExceptionVector::try_from(self.vector) {
            Ok(ExceptionVector::Breakpoint) => ExceptionKind::Breakpoint,
            Ok(ExceptionVector::InvalidOpcode) => ExceptionKind::IllegalInstruction,
            _ => ExceptionKind::Other,
        }
    }
}

/// Initializes syscall support and setups the syscall handler.
pub(super) fn init_syscall() {
    unsafe extern "C" {
        unsafe fn syscall_entry();
    }

    LStar::write(x86_64::VirtAddr::new_truncate(
        syscall_entry as *const () as usize as _,
    ));
    Star::write(gdt::UCODE64, gdt::UDATA, gdt::KCODE64, gdt::KDATA).unwrap();
    SFMask::write(
        RFlags::TRAP_FLAG
            | RFlags::INTERRUPT_FLAG
            | RFlags::DIRECTION_FLAG
            | RFlags::IOPL_LOW
            | RFlags::IOPL_HIGH
            | RFlags::NESTED_TASK
            | RFlags::ALIGNMENT_CHECK,
    ); // TF | IF | DF | IOPL | AC | NT (0x47700)
    // SAFETY: Enabling the SCE (System Call Extensions) bit in EFER. `syscall_entry` is defined in `excp.S` and resides in kernel code space. Segment selectors in `Star` are compile-time constants from `gdt.rs`.
    unsafe {
        Efer::update(|efer| *efer |= EferFlags::SYSTEM_CALL_EXTENSIONS);
    }
}
