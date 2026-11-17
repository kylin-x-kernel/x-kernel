// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 signal frame layout and trampoline.
use kcpu::userspace::UserContext;
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
    mov x8, #139
    svc #0

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
struct MContextPadding([u8; 4096]);

#[repr(C)]
#[derive(Clone)]
/// Machine context in the Linux AArch64 `mcontext_t` ABI.
pub struct MContext {
    fault_address: u64,
    regs: [u64; 31],
    sp: u64,
    pc: u64,
    pstate: u64,
    abi_pad: [u8; 8],
    __reserved: MContextPadding,
}

const _: () = {
    assert!(core::mem::offset_of!(MContext, __reserved) == 288);
    assert!(size_of::<MContext>() == 4384);
};

impl MContext {
    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            fault_address: 0,
            regs: uctx.x,
            sp: uctx.sp,
            pc: uctx.elr,
            pstate: uctx.spsr,
            abi_pad: [0; 8],
            __reserved: MContextPadding([0; 4096]),
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.x = self.regs;
        uctx.sp = self.sp;
        uctx.elr = self.pc;
        uctx.spsr = self.pstate;
    }
}

/// User-visible `ucontext_t` frame for AArch64 signal handlers.
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
    assert!(core::mem::offset_of!(UContext, stack) == 16);
    assert!(core::mem::offset_of!(UContext, mcontext) == 176);
    assert!(size_of::<UContext>() == 4560);
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
