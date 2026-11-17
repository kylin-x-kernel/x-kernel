// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 signal frame layout and trampoline.
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
    li.w    $a7, 139
    syscall 0

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
/// Machine context in the Linux LoongArch `mcontext_t` ABI.
pub struct MContext {
    sc_pc: u64,
    sc_regs: GeneralRegisters,
    sc_flags: u32,
    abi_pad: [u8; 4],
}

const _: () = {
    assert!(size_of::<MContext>() == 272);
    assert!(core::mem::offset_of!(MContext, abi_pad) == 268);
};

impl MContext {
    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            sc_pc: uctx.era as _,
            sc_regs: uctx.regs,
            sc_flags: 0,
            abi_pad: [0; 4],
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.era = self.sc_pc as _;
        uctx.regs = self.sc_regs;
    }
}

/// User-visible `ucontext_t` frame for LoongArch signal handlers.
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
    assert!(size_of::<UContext>() == 448);
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
