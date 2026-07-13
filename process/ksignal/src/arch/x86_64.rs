// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 signal frame layout and trampoline.
use kcpu::userspace::UserContext;

use crate::{SignalSet, SignalStack};

/// Stack alignment required when entering a user signal handler.
pub(crate) const SIGNAL_FRAME_ALIGN: usize = 16;

core::arch::global_asm!(
    "
.section .text
.code64
.balign 4096
.global signal_trampoline
signal_trampoline:
    mov rax, 0xf
    syscall

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C)]
#[derive(Clone)]
pub struct MContext {
    // Match Linux x86_64/musl `mcontext_t` so user-space signal handlers can
    // read and edit `uc_mcontext.gregs[...]` (notably `MC_PC` for cancellation).
    gregs: [i64; 23],
    fpregs: usize,
    _reserved1: [u64; 8],
}

impl MContext {
    const REG_CR2: usize = 22;
    const REG_CSGSFS: usize = 18;
    const REG_EFL: usize = 17;
    const REG_ERR: usize = 19;
    const REG_OLDMASK: usize = 21;
    const REG_R10: usize = 2;
    const REG_R11: usize = 3;
    const REG_R12: usize = 4;
    const REG_R13: usize = 5;
    const REG_R14: usize = 6;
    const REG_R15: usize = 7;
    const REG_R8: usize = 0;
    const REG_R9: usize = 1;
    const REG_RAX: usize = 13;
    const REG_RBP: usize = 10;
    const REG_RBX: usize = 11;
    const REG_RCX: usize = 14;
    const REG_RDI: usize = 8;
    const REG_RDX: usize = 12;
    const REG_RIP: usize = 16;
    const REG_RSI: usize = 9;
    const REG_RSP: usize = 15;
    const REG_TRAPNO: usize = 20;

    fn pack_cs(uctx: &UserContext) -> i64 {
        uctx.cs as i64
    }

    /// Returns the instruction pointer recorded in this machine context.
    pub fn ip(&self) -> usize {
        self.gregs[Self::REG_RIP] as usize
    }

    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        let mut gregs = [0_i64; 23];
        gregs[Self::REG_R8] = uctx.r8 as i64;
        gregs[Self::REG_R9] = uctx.r9 as i64;
        gregs[Self::REG_R10] = uctx.r10 as i64;
        gregs[Self::REG_R11] = uctx.r11 as i64;
        gregs[Self::REG_R12] = uctx.r12 as i64;
        gregs[Self::REG_R13] = uctx.r13 as i64;
        gregs[Self::REG_R14] = uctx.r14 as i64;
        gregs[Self::REG_R15] = uctx.r15 as i64;
        gregs[Self::REG_RDI] = uctx.rdi as i64;
        gregs[Self::REG_RSI] = uctx.rsi as i64;
        gregs[Self::REG_RBP] = uctx.rbp as i64;
        gregs[Self::REG_RBX] = uctx.rbx as i64;
        gregs[Self::REG_RDX] = uctx.rdx as i64;
        gregs[Self::REG_RAX] = uctx.rax as i64;
        gregs[Self::REG_RCX] = uctx.rcx as i64;
        gregs[Self::REG_RSP] = uctx.rsp as i64;
        gregs[Self::REG_RIP] = uctx.rip as i64;
        gregs[Self::REG_EFL] = uctx.rflags as i64;
        gregs[Self::REG_CSGSFS] = Self::pack_cs(uctx);
        gregs[Self::REG_ERR] = uctx.error_code as i64;
        gregs[Self::REG_TRAPNO] = uctx.vector as i64;
        gregs[Self::REG_OLDMASK] = 0;
        gregs[Self::REG_CR2] = 0;

        Self {
            gregs,
            fpregs: 0,
            _reserved1: [0; 8],
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.r8 = self.gregs[Self::REG_R8] as _;
        uctx.r9 = self.gregs[Self::REG_R9] as _;
        uctx.r10 = self.gregs[Self::REG_R10] as _;
        uctx.r11 = self.gregs[Self::REG_R11] as _;
        uctx.r12 = self.gregs[Self::REG_R12] as _;
        uctx.r13 = self.gregs[Self::REG_R13] as _;
        uctx.r14 = self.gregs[Self::REG_R14] as _;
        uctx.r15 = self.gregs[Self::REG_R15] as _;
        uctx.rdi = self.gregs[Self::REG_RDI] as _;
        uctx.rsi = self.gregs[Self::REG_RSI] as _;
        uctx.rbp = self.gregs[Self::REG_RBP] as _;
        uctx.rbx = self.gregs[Self::REG_RBX] as _;
        uctx.rdx = self.gregs[Self::REG_RDX] as _;
        uctx.rax = self.gregs[Self::REG_RAX] as _;
        uctx.rcx = self.gregs[Self::REG_RCX] as _;
        uctx.rsp = self.gregs[Self::REG_RSP] as _;
        uctx.rip = self.gregs[Self::REG_RIP] as _;
        uctx.rflags = self.gregs[Self::REG_EFL] as _;
        uctx.cs = (self.gregs[Self::REG_CSGSFS] as u64 & 0xffff) as _;
        uctx.error_code = self.gregs[Self::REG_ERR] as _;
        uctx.vector = self.gregs[Self::REG_TRAPNO] as _;
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct UContext {
    pub flags: usize,
    pub link: usize,
    pub stack: SignalStack,
    pub mcontext: MContext,
    pub sigmask: SignalSet,
    pub fpregs_mem: [u64; 64],
}

impl UContext {
    /// Build a user context frame for signal handling.
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default(),
            mcontext: MContext::new(uctx),
            sigmask,
            fpregs_mem: [0; 64],
        }
    }
}
