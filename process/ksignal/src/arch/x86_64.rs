// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 signal frame layout and trampoline.
use kcpu::userspace::UserContext;
use kerrno::{KError, KResult};
use posix_types::{UserPtr, UserRead, UserWrite, k_siginfo};

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
#[derive(Clone, Copy)]
/// Machine context in the Linux x86_64 `mcontext_t` ABI.
///
/// The register array is kept private so callers use
/// [`MContext::new`]/[`MContext::restore`] instead of hand-packing gregs.
pub struct MContext {
    // Match Linux x86_64/musl `mcontext_t` so user-space signal handlers can
    // read and edit `uc_mcontext.gregs[...]` (notably `MC_PC` for cancellation).
    gregs: [i64; 23],
    /// User address of a legacy FXSAVE or Linux XSAVE signal payload.
    pub fpregs: usize,
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

    /// Checks the supported 64-bit user return ABI before changing registers.
    pub fn validate(&self) -> KResult<()> {
        let canonical = |value: i64| value >= 0 && (value as u64) < (1u64 << 47);
        if !canonical(self.gregs[Self::REG_RIP])
            || !canonical(self.gregs[Self::REG_RSP])
            || self.gregs[Self::REG_CSGSFS] & 0xffff != UserContext::USER_CODE_SELECTOR as i64
        {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    /// Restore a validated user context from this machine context.
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
        // Linux FIX_EFLAGS: only arithmetic/status/debug user flags may change.
        const USER_FLAGS: u64 = 0x50dd5;
        uctx.rflags = (uctx.rflags & !USER_FLAGS) | (self.gregs[Self::REG_EFL] as u64 & USER_FLAGS);
        // This kernel supports only 64-bit user code, never kernel selectors.
        uctx.cs = UserContext::USER_CODE_SELECTOR;
        uctx.ss = UserContext::USER_DATA_SELECTOR;
        uctx.orig_rax = u64::MAX;
        uctx.error_code = self.gregs[Self::REG_ERR] as _;
        uctx.vector = self.gregs[Self::REG_TRAPNO] as _;
    }
}

/// User-visible `ucontext_t` frame for x86_64 signal handlers (musl layout).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UContext {
    /// `uc_flags`; UC_FP_XSTATE is set when the FP pointer names XSAVE state.
    pub flags: usize,
    /// `uc_link`; not used by signal frames and written as zero.
    pub link: usize,
    /// `uc_stack` snapshot of the alternate stack state.
    pub stack: SignalStackAbi,
    /// `uc_mcontext` general-purpose register snapshot.
    pub mcontext: MContext,
    /// `uc_sigmask` restored into the thread blocked set at `sigreturn`.
    pub sigmask: SignalSet,
}

impl UContext {
    /// Build a user context frame for signal handling.
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStackAbi::from_stack(&SignalStack::default(), 0),
            mcontext: MContext::new(uctx),
            sigmask,
        }
    }
}

/// Linux x86_64 stack_t layout, including explicit initialized padding.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SignalStackAbi {
    sp: usize,
    flags: u32,
    padding: u32,
    size: usize,
}

impl SignalStackAbi {
    fn from_stack(stack: &SignalStack, sp: usize) -> Self {
        Self {
            sp: stack.sp,
            flags: stack.flags_for_sp(sp),
            padding: 0,
            size: stack.size,
        }
    }
}

// SAFETY: repr(C) UContext consists only of integer fields, integer arrays,
// and SignalSet's transparent u64. Every copied bit pattern is representable;
// user registers and xstate are validated separately before hardware use.
unsafe impl UserRead for UContext {}

// SAFETY: the same repr(C) integer-only fields leave no implicit holes: the
// stack_t gap is the explicit zeroed `SignalStackAbi::padding`, and the
// assertions below pin Linux ucontext offsets and the 304-byte total.
unsafe impl UserWrite for UContext {}

const _: () = {
    assert!(core::mem::size_of::<SignalStackAbi>() == 24);
    assert!(core::mem::size_of::<MContext>() == 256);
    assert!(core::mem::offset_of!(UContext, stack) == 16);
    assert!(core::mem::offset_of!(UContext, mcontext) == 40);
    assert!(core::mem::offset_of!(UContext, sigmask) == 296);
    assert!(core::mem::size_of::<UContext>() == 304);
};

/// Writes the Linux x86_64 rt frame at the signal-delivery user boundary.
/// Each region is copied separately, so alignment gaps never expose kernel
/// stack padding. The FP pointer is 64-byte aligned; handler RSP is 8 mod 16.
pub(crate) fn setup_signal_frame(
    uctx: &mut UserContext,
    mask: SignalSet,
    sig: &crate::SignalInfo,
    stack: &SignalStack,
    sp: usize,
    restorer: usize,
) -> KResult<(usize, usize)> {
    let (fp, fp_len) = kcpu::ExtendedState::capture().signal_bytes();
    let fp_addr = sp.checked_sub(128 + fp_len).ok_or(KError::BadAddress)? & !63;
    let info_addr = fp_addr
        .checked_sub(core::mem::size_of::<crate::SignalInfo>())
        .ok_or(KError::BadAddress)?;
    let uc_addr = info_addr
        .checked_sub(core::mem::size_of::<UContext>())
        .ok_or(KError::BadAddress)?
        & !(SIGNAL_FRAME_ALIGN - 1);
    let handler_sp = uc_addr.checked_sub(8).ok_or(KError::BadAddress)?;
    if !stack.disabled()
        && (stack.contains_sp(uctx.sp()) || sp == stack.sp.saturating_add(stack.size))
        && handler_sp < stack.sp
    {
        return Err(KError::BadAddress);
    }
    let mut uc = UContext::new(uctx, mask);
    uc.stack = SignalStackAbi::from_stack(stack, uctx.sp());
    uc.flags = usize::from(kcpu::enabled_xstate_mask() != 0);
    uc.mcontext.fpregs = fp_addr;
    UserPtr::<u8>::from(fp_addr).write_vm_slice(&fp[..fp_len])?;
    UserPtr::<k_siginfo>::from(info_addr).write_vm(k_siginfo::from(sig.clone()))?;
    UserPtr::<UContext>::from(uc_addr).write_vm(uc)?;
    UserPtr::<usize>::from(handler_sp).write_vm(restorer)?;
    uctx.set_sp(handler_sp);
    // A handler gets clean FP state; the interrupted state lives in its frame.
    kcpu::userspace::reset_fp_state();
    Ok((info_addr, uc_addr))
}

unittest::path_mod_test!(tests, "../../ut-tests/x86_frame.rs");
