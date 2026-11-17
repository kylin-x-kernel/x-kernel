// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V signal frame layout and trampoline.
use kcpu::{GeneralRegisters, userspace::UserContext};
use posix_types::k_sigaltstack;

use crate::{SignalSet, SignalStack};

/// Stack alignment required when entering a user signal handler.
pub(crate) const SIGNAL_FRAME_ALIGN: usize = 16;

core::arch::global_asm!(
    "
.section .text
.balign 4096
.global signal_trampoline
signal_trampoline:
    li a7, 139
    ecall

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
/// Machine context in the Linux RISC-V `mcontext_t` ABI.
pub struct MContext {
    /// Program counter (`pc`), the first word of Linux `user_regs_struct`.
    pub pc: usize,
    /// x1..x31 in Linux `user_regs_struct` order (x0 is not stored).
    regs: GeneralRegisters,
    fpstate: [usize; 66],
}

// Linux RISC-V sigcontext: 32-word user_regs_struct (PC, x1..x31) followed by
// the 528-byte FP union; no implicit padding between or after them.
const _: () = {
    assert!(size_of::<GeneralRegisters>() == 31 * size_of::<usize>());
    assert!(core::mem::offset_of!(MContext, regs) == size_of::<usize>());
    assert!(core::mem::offset_of!(MContext, fpstate) == 256);
    assert!(size_of::<MContext>() == 784);
};

impl MContext {
    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            pc: uctx.sepc,
            regs: uctx.regs,
            fpstate: [0; 66],
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.sepc = self.pc;
        uctx.regs = self.regs;
    }
}

/// User-visible `ucontext_t` frame for RISC-V signal handlers.
#[repr(C)]
#[derive(Clone)]
pub struct UContext {
    /// `uc_flags`; written as zero by the kernel.
    pub flags: usize,
    /// `uc_link`; not used by signal frames and written as zero.
    pub link: usize,
    /// `uc_stack` snapshot of the alternate stack state.
    pub stack: k_sigaltstack,
    /// `uc_sigmask` restored into the thread blocked set at `sigreturn`.
    pub sigmask: SignalSet,
    __unused: [u8; 1024 / 8 - size_of::<SignalSet>()],
    /// Zeroed hole before the 16-byte-aligned `uc_mcontext`.
    abi_pad: [u8; 8],
    /// `uc_mcontext` general-purpose register snapshot.
    pub mcontext: MContext,
}

const _: () = {
    assert!(core::mem::offset_of!(UContext, mcontext) == 176);
    assert!(size_of::<UContext>() == 960);
};

impl UContext {
    /// Build a user context frame for signal handling.
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default().into(),
            sigmask,
            __unused: [0; 1024 / 8 - size_of::<SignalSet>()],
            abi_pad: [0; 8],
            mcontext: MContext::new(uctx),
        }
    }
}
