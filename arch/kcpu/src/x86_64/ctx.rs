// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 context structures for traps and task switching.

use core::{arch::naked_asm, fmt};

use memaddr::VirtAddr;

/// Saved registers when a trap (interrupt or exception) occurs.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ExceptionContext {
    /// General-purpose register rax.
    pub rax: u64,
    /// General-purpose register rcx.
    pub rcx: u64,
    /// General-purpose register rdx.
    pub rdx: u64,
    /// General-purpose register rbx.
    pub rbx: u64,
    /// Base pointer register rbp.
    pub rbp: u64,
    /// Source index register rsi.
    pub rsi: u64,
    /// Destination index register rdi.
    pub rdi: u64,
    /// General-purpose register r8.
    pub r8: u64,
    /// General-purpose register r9.
    pub r9: u64,
    /// General-purpose register r10.
    pub r10: u64,
    /// General-purpose register r11.
    pub r11: u64,
    /// General-purpose register r12.
    pub r12: u64,
    /// General-purpose register r13.
    pub r13: u64,
    /// General-purpose register r14.
    pub r14: u64,
    /// General-purpose register r15.
    pub r15: u64,

    /// Trap vector number (pushed by `trap.S`).
    pub vector: u64,
    /// Error code (pushed by `trap.S` or CPU depending on vector).
    pub error_code: u64,

    /// Instruction pointer at trap time.
    pub rip: u64,
    /// Code segment selector.
    pub cs: u64,
    /// RFLAGS register.
    pub rflags: u64,
    /// Stack pointer at trap time.
    pub rsp: u64,
    /// Stack segment selector.
    pub ss: u64,
}

impl ExceptionContext {
    /// Gets the 0th syscall argument.
    pub const fn arg0(&self) -> usize {
        self.rdi as _
    }

    /// Sets the 0th syscall argument.
    pub const fn set_arg0(&mut self, rdi: usize) {
        self.rdi = rdi as _;
    }

    /// Gets the 1st syscall argument.
    pub const fn arg1(&self) -> usize {
        self.rsi as _
    }

    /// Sets the 1st syscall argument.
    pub const fn set_arg1(&mut self, rsi: usize) {
        self.rsi = rsi as _;
    }

    /// Gets the 2nd syscall argument.
    pub const fn arg2(&self) -> usize {
        self.rdx as _
    }

    /// Sets the 2nd syscall argument.
    pub const fn set_arg2(&mut self, rdx: usize) {
        self.rdx = rdx as _;
    }

    /// Gets the 3rd syscall argument.
    pub const fn arg3(&self) -> usize {
        self.r10 as _
    }

    /// Sets the 3rd syscall argument.
    pub const fn set_arg3(&mut self, r10: usize) {
        self.r10 = r10 as _;
    }

    /// Gets the 4th syscall argument.
    pub const fn arg4(&self) -> usize {
        self.r8 as _
    }

    /// Sets the 4th syscall argument.
    pub const fn set_arg4(&mut self, r8: usize) {
        self.r8 = r8 as _;
    }

    /// Gets the 5th syscall argument.
    pub const fn arg5(&self) -> usize {
        self.r9 as _
    }

    /// Sets the 5th syscall argument.
    pub const fn set_arg5(&mut self, r9: usize) {
        self.r9 = r9 as _;
    }

    /// Gets the instruction pointer.
    pub const fn ip(&self) -> usize {
        self.rip as _
    }

    /// Sets the instruction pointer.
    pub const fn set_ip(&mut self, rip: usize) {
        self.rip = rip as _;
    }

    /// Gets the stack pointer.
    pub const fn sp(&self) -> usize {
        self.rsp as _
    }

    /// Sets the stack pointer.
    pub const fn set_sp(&mut self, rsp: usize) {
        self.rsp = rsp as _;
    }

    /// Gets the syscall number.
    pub const fn sysno(&self) -> usize {
        self.rax as usize
    }

    /// Sets the syscall number.
    pub const fn set_sysno(&mut self, rax: usize) {
        self.rax = rax as _;
    }

    /// Gets the return value register.
    pub const fn retval(&self) -> usize {
        self.rax as _
    }

    /// Sets the return value register.
    pub const fn set_retval(&mut self, rax: usize) {
        self.rax = rax as _;
    }

    /// Unwind the stack and get the backtrace.
    pub fn backtrace(&self) -> backtrace::Backtrace {
        backtrace::Backtrace::capture_trap(self.rbp as _, self.rip as _, 0)
    }
}

#[repr(C)]
#[derive(Debug, Default)]
struct ContextSwitchFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbx: u64,
    rbp: u64,
    rip: u64,
}

/// A 512-byte memory region for the FXSAVE/FXRSTOR instruction to save and
/// restore the x87 FPU, MMX, XMM, and MXCSR registers.
///
/// See <https://www.felixcloutier.com/x86/fxsave> for more details.
#[repr(C, align(16))]
#[derive(Debug)]
pub struct FxsaveArea {
    /// FPU control word.
    pub fcw: u16,
    /// FPU status word.
    pub fsw: u16,
    /// FPU tag word.
    pub ftw: u16,
    /// FPU opcode.
    pub fop: u16,
    /// FPU instruction pointer.
    pub fip: u64,
    /// FPU data pointer.
    pub fdp: u64,
    /// SSE control and status register.
    pub mxcsr: u32,
    /// MXCSR mask.
    pub mxcsr_mask: u32,
    /// x87/MMX registers.
    pub st: [u64; 16],
    /// XMM registers.
    pub xmm: [u64; 32],
    /// Reserved padding.
    _padding: [u64; 12],
}

static_assertions::const_assert_eq!(core::mem::size_of::<FxsaveArea>(), 512);

/// Extended state of a task, such as FP/SIMD states.
pub struct ExtendedState {
    /// Memory region for the FXSAVE/FXRSTOR instruction.
    pub fxsave_area: FxsaveArea,
}

#[cfg(feature = "fp-simd")]
impl ExtendedState {
    /// Saves the current extended states from CPU to this structure.
    #[inline]
    pub fn save(&mut self) {
        unsafe { core::arch::x86_64::_fxsave64(&mut self.fxsave_area as *mut _ as *mut u8) }
    }

    /// Restores the extended states from this structure to CPU.
    #[inline]
    pub fn restore(&self) {
        unsafe { core::arch::x86_64::_fxrstor64(&self.fxsave_area as *const _ as *const u8) }
    }

    /// Returns the extended state with initialized values.
    pub const fn default() -> Self {
        let mut area: FxsaveArea = unsafe { core::mem::MaybeUninit::zeroed().assume_init() };
        area.fcw = 0x37f;
        area.ftw = 0xffff;
        area.mxcsr = 0x1f80;
        Self { fxsave_area: area }
    }
}

impl fmt::Debug for ExtendedState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ExtendedState")
            .field("fxsave_area", &self.fxsave_area)
            .finish()
    }
}

/// Saved hardware states of a task.
///
/// The context usually includes:
///
/// - Callee-saved registers
/// - Stack pointer register
/// - Thread pointer register (for kernel-space thread-local storage)
/// - FP/SIMD registers
///
/// On context switch, current task saves its context from CPU to memory,
/// and the next task restores its context from memory to CPU.
///
/// On x86_64, callee-saved registers are saved to the kernel stack by the
/// `PUSH` instruction. So that [`rsp`] is the `RSP` after callee-saved
/// registers are pushed, and [`kstack_top`] is the top of the kernel stack
/// (`RSP` before any push).
///
/// [`rsp`]: TaskContext::rsp
/// [`kstack_top`]: TaskContext::kstack_top
#[derive(Debug)]
pub struct TaskContext {
    /// The kernel stack top of the task.
    pub kstack_top: VirtAddr,
    /// `RSP` after all callee-saved registers are pushed.
    pub rsp: u64,
    /// Thread pointer (FS segment base address)
    pub fs_base: usize,
    /// Extended states, i.e., FP/SIMD states.
    #[cfg(feature = "fp-simd")]
    pub ext_state: ExtendedState,
    /// The `CR3` register value, i.e., the page table root.
    #[cfg(feature = "uspace")]
    pub cr3: memaddr::PhysAddr,
}

impl TaskContext {
    /// Creates a dummy context for a new task.
    ///
    /// Note the context is not initialized, it will be filled by [`switch_to`]
    /// (for initial tasks) and [`init`] (for regular tasks) methods.
    ///
    /// [`init`]: TaskContext::init
    /// [`switch_to`]: TaskContext::switch_to
    pub fn new() -> Self {
        Self {
            kstack_top: va!(0),
            rsp: 0,
            fs_base: 0,
            #[cfg(feature = "uspace")]
            cr3: crate::instrs::read_kernel_page_table(),
            #[cfg(feature = "fp-simd")]
            ext_state: ExtendedState::default(),
        }
    }

    /// Initializes the context for a new task, with the given entry point and
    /// kernel stack.
    pub fn init(&mut self, entry: usize, kstack_top: VirtAddr, tls_area: VirtAddr) {
        unsafe {
            // x86_64 calling convention: the stack must be 16-byte aligned before
            // calling a function. That means when entering a new task (`ret` in `context_switch`
            // is executed), (stack pointer + 8) should be 16-byte aligned.
            let frame_ptr = (kstack_top.as_mut_ptr() as *mut u64).sub(1);
            let frame_ptr = (frame_ptr as *mut ContextSwitchFrame).sub(1);
            core::ptr::write(
                frame_ptr,
                ContextSwitchFrame {
                    rip: entry as _,
                    ..Default::default()
                },
            );
            self.rsp = frame_ptr as u64;
        }
        self.kstack_top = kstack_top;
        self.fs_base = tls_area.as_usize();
    }

    /// Changes the page table root in this context.
    ///
    /// The hardware register for page table root (`CR3` for x86) will be
    /// updated to the next task's after [`Self::switch_to`].
    #[cfg(feature = "uspace")]
    pub fn set_page_table_root(&mut self, cr3: memaddr::PhysAddr) {
        self.cr3 = cr3;
    }

    /// Switches to another task.
    ///
    /// It first saves the current task's context from CPU to this place, and then
    /// restores the next task's context from `next_ctx` to CPU.
    pub fn switch_to(&mut self, next_ctx: &Self) {
        #[cfg(feature = "fp-simd")]
        {
            self.ext_state.save();
            next_ctx.ext_state.restore();
        }
        #[cfg(feature = "tls")]
        unsafe {
            self.fs_base = crate::instrs::read_thread_pointer();
            crate::instrs::write_thread_pointer(next_ctx.fs_base);
        }
        #[cfg(feature = "uspace")]
        unsafe {
            if next_ctx.cr3 != self.cr3 {
                crate::instrs::write_user_page_table(next_ctx.cr3);
                // writing to CR3 has flushed the TLB
            }
        }
        unsafe { context_switch(&mut self.rsp, &next_ctx.rsp) }
    }
}

#[unsafe(naked)]
unsafe extern "C" fn context_switch(_current_stack: &mut u64, _next_stack: &u64) {
    naked_asm!(
        "
        .code64
        push    rbp
        push    rbx
        push    r12
        push    r13
        push    r14
        push    r15
        mov     [rdi], rsp

        mov     rsp, [rsi]
        pop     r15
        pop     r14
        pop     r13
        pop     r12
        pop     rbx
        pop     rbp
        ret",
    )
}
