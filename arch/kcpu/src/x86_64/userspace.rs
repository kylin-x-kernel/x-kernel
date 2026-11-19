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
    ExtendedState, TrapFrame,
    excp::{IRQ_VECTOR_END, IRQ_VECTOR_START, LEGACY_SYSCALL_VECTOR, err_code_to_flags},
    gdt,
};
pub use crate::userspace_common::{ExceptionKind, ReturnReason};

kcpu_slot::cpu_slot_cell! {
    /// Per-CPU cache of the FS base currently programmed into hardware.
    ///
    /// The kernel never uses `FS_BASE`, so the user value can stay in hardware
    /// across syscalls and context switches; this only lets the return path skip
    /// a redundant `wrmsr` when the value is already current. `u64::MAX` means
    /// "unknown", forcing the first write on this CPU.
    static APPLIED_FS_BASE: u64 = u64::MAX;
}
kcpu_slot::cpu_slot_cell! {
    /// Per-CPU cache of `KERNEL_GS_BASE`, which holds the user GS base while
    /// the kernel runs (the value `swapgs` moves back into `GS_BASE` on return).
    static APPLIED_GS_BASE: u64 = u64::MAX;
}

/// Context to enter user space.
///
/// `kernel_rsp` sits immediately before `tf` on purpose: the entry assembly
/// treats `&kernel_rsp` as its private frame, storing the Rust caller's kernel
/// stack there and using `tf` in place as the temporary trap stack (TSS.rsp0
/// points to the end of `tf`). Keeping both in this struct lets the user return
/// avoid copying the whole trap frame around the user window.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug)]
pub struct UserContext {
    kernel_rsp: u64,
    tf: TrapFrame,
    /// FS Segment Base
    pub fs_base: u64,
    /// GS Segment Base
    pub gs_base: u64,
}

/// Kernel-owned state captured when preparing a new user task.
///
/// Segment bases accompany the x87/SSE/YMM snapshot inherited by fork/clone.
/// Signal delivery instead exports the Linux FP payload through `fpregs`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserRestorableContext {
    fs_base: u64,
    gs_base: u64,
    fp: ExtendedState,
}

pub(super) const KERNEL_RSP_TO_TRAPFRAME_OFFSET: usize =
    core::mem::offset_of!(UserContext, tf) - core::mem::offset_of!(UserContext, kernel_rsp);

const _: () = assert!(KERNEL_RSP_TO_TRAPFRAME_OFFSET == core::mem::size_of::<u64>());

/// Installs clean x87/SSE/YMM state for a signal handler or new executable.
pub fn reset_fp_state() {
    #[cfg(feature = "fp-simd")]
    ExtendedState::default().restore();
}

/// Restores a validated, kernel-owned FP snapshot on signal return.
pub fn restore_fp_state(state: &ExtendedState) {
    #[cfg(feature = "fp-simd")]
    state.restore();
}

impl UserContext {
    /// Code selector installed by this kernel's 64-bit user GDT entry.
    pub const USER_CODE_SELECTOR: u64 = gdt::UCODE64.0 as u64;
    /// Data/stack selector installed by this kernel's user GDT entry.
    pub const USER_DATA_SELECTOR: u64 = gdt::UDATA.0 as u64;

    /// Creates a new context with the given entry point, user stack pointer,
    /// and the argument.
    pub fn new(entry: usize, ustack_top: VirtAddr, arg0: usize) -> Self {
        use x86_64::registers::rflags::RFlags;
        Self {
            kernel_rsp: 0,
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

    /// Captures the creator's live FP state and the new task's segment bases.
    ///
    /// Call while the creator is current, before publishing the new task.
    pub fn save_user_restorable(&self) -> UserRestorableContext {
        let fp = ExtendedState::capture();
        UserRestorableContext {
            fs_base: self.fs_base,
            gs_base: self.gs_base,
            fp,
        }
    }

    /// Installs a kernel-owned snapshot on the new task's first user entry.
    ///
    /// No user memory is read here. Signal return uses validated FP decoding
    /// and does not restore FS/GS bases from a private signal-frame extension.
    pub fn restore_user_restorable(&mut self, saved: UserRestorableContext) {
        self.fs_base = saved.fs_base;
        self.gs_base = saved.gs_base;
        self.ss = gdt::UDATA.0 as _;
        self.orig_rax = u64::MAX;

        #[cfg(feature = "fp-simd")]
        {
            saved.fp.restore();
        }
    }

    /// Sets the TLS area.
    pub const fn set_tls(&mut self, tls_area: usize) {
        self.fs_base = tls_area as _;
    }

    /// Resets the general-purpose registers before entering a freshly
    /// loaded executable.
    ///
    /// A successful `execve` must not leak the old program's registers into
    /// the new image. On x86-64 the third `execve` argument `envp` sits in
    /// `rdx`; glibc's static `_start` passes `rdx` to `__libc_start_main` as
    /// the `rtld_fini` callback, which must be zero for a statically linked
    /// binary. A stale value would be registered as an exit handler and then
    /// called from `__run_exit_handlers`, faulting into the old heap.
    ///
    /// The x86-64 ELF entry ABI only defines `rsp`, `rip`, and (for dynamic
    /// entry) `rdx`, so every other general-purpose register is cleared along
    /// with the user GS base and the "not from syscall" marker. Control fields
    /// (`cs`/`ss`/`rflags`, `rip`, `rsp`, `fs_base`) are left for the caller
    /// to re-establish via `set_ip`/`set_sp`/`set_tls`.
    pub fn reset_for_exec(&mut self) {
        #[cfg(feature = "fp-simd")]
        ExtendedState::default().restore();
        self.rax = 0;
        self.rcx = 0;
        self.rdx = 0;
        self.rbx = 0;
        self.rbp = 0;
        self.rsi = 0;
        self.rdi = 0;
        self.r8 = 0;
        self.r9 = 0;
        self.r10 = 0;
        self.r11 = 0;
        self.r12 = 0;
        self.r13 = 0;
        self.r14 = 0;
        self.r15 = 0;
        self.orig_rax = u64::MAX;
        self.gs_base = 0;
    }

    /// Enters user space.
    ///
    /// It restores the user registers and jumps to the user entry point
    /// (saved in `rip`).
    ///
    /// This function returns when an exception or syscall occurs.
    ///
    /// Invokes `on_user_enter` immediately before entering user space and
    /// `on_user_exit` after restoring kernel context, before IRQ dispatch.
    /// Both callbacks run with local IRQs masked and must return with them
    /// masked, without sleeping or scheduling.
    /// Between callbacks, kernel transfer code accesses only live context and
    /// register state: no scheduling or RCU-protected lookup is permitted.
    pub fn run(
        &mut self,
        on_user_enter: impl FnOnce(),
        on_user_exit: impl FnOnce(),
    ) -> ReturnReason {
        unsafe extern "C" {
            unsafe fn enter_user(frame: &mut UserContext);
        }

        assert_eq!(self.cs, gdt::UCODE64.0 as _);
        assert_eq!(self.ss, gdt::UDATA.0 as _);

        karch::disable_local_irq();

        on_user_enter();
        // The kernel does not use `FS_BASE`, and every path that changes the
        // user bases (`arch_prctl`, exec, signal restore) updates these fields
        // directly, so nothing needs to be read back from hardware. Only publish
        // a base when this CPU is not already running with it, which makes a
        // syscall loop on one task free of MSR accesses entirely.
        if APPLIED_FS_BASE.load_current() != self.fs_base {
            // SAFETY: `self.fs_base` is the validated user TLS base for this thread.
            unsafe { karch::write_thread_pointer(self.fs_base as _) };
            APPLIED_FS_BASE.store_current(self.fs_base);
        }
        if APPLIED_GS_BASE.load_current() != self.gs_base {
            KernelGsBase::write(x86_64::VirtAddr::new_truncate(self.gs_base));
            APPLIED_GS_BASE.store_current(self.gs_base);
        }

        // SAFETY: `enter_user` uses `&self.kernel_rsp` as its private frame and
        // `self.tf` in place as the user trap stack. `self.tf` was initialized
        // by `new()` or restored from validated signal state with user segment
        // selectors, and the hardware trap frame stays inside `tf`.
        unsafe { enter_user(self) };

        on_user_exit();

        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: kernel four-level paging records \
                      canonical page-fault addresses in CR2; no kernel path writes a noncanonical \
                      CR2 value"
        )]
        let cr2 = Cr2::read().unwrap().as_u64() as usize;
        let vector = self.vector as u8;

        const PAGE_FAULT_VECTOR: u8 = ExceptionVector::Page as u8;

        let ret = match vector {
            PAGE_FAULT_VECTOR if let Ok(flags) = err_code_to_flags(self.error_code) => {
                ReturnReason::PageFault(va!(cr2), flags)
            }
            LEGACY_SYSCALL_VECTOR => ReturnReason::Syscall,
            IRQ_VECTOR_START..=IRQ_VECTOR_END => {
                // The user-return entry dispatches here instead of the kernel
                // trap handler, so publish the interrupted user frame as this
                // CPU's active trap context for tick classification.
                let _tf_guard = crate::ExceptionContextGuard::new(&self.tf);
                // SAFETY: the user interrupt gate cleared IF and enter_user
                // restored kernel CPU state before this saved-vector dispatch.
                unsafe { handle_trap!(IRQ, vector as _) };
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
#[derive(Clone, Copy, Debug)]
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
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: the fixed GDT selectors satisfy STAR \
                  ordering: UCODE64 follows UDATA and KDATA follows KCODE64"
    )]
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

#[cfg(all(unittest, target_arch = "x86_64"))]
unittest::path_mod_test!(tests, "../../ut-tests/x86_64_userspace.rs");
