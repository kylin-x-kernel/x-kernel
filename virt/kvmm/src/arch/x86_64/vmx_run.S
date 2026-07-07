// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// x86_64 VMX guest entry/exit assembly.
//
// vmx_enter_guest(vcpu: *mut X86Vcpu) -> i32
//   Saves host callee-saved, writes HOST_RSP/HOST_RIP to VMCS,
//   loads guest GPRs, executes vmlaunch/vmresume.
//   Returns 1 on successful VM-exit, 0 on vmlaunch/vmresume failure.
//
// vmx_return
//   VM-exit landing point (HOST_RIP). Saves guest GPRs to vcpu,
//   restores host callee-saved, returns 1.
//
// X86Vcpu layout (offsets from struct start):
//   rax +0x00  rbx +0x08  rcx +0x10  rdx +0x18
//   rbp +0x20  rsi +0x28  rdi +0x30
//   r8  +0x38  r9  +0x40  r10 +0x48  r11 +0x50
//   r12 +0x58  r13 +0x60  r14 +0x68  r15 +0x70
//   rflags +0x78
//
// Vcpu<X86Vmx> layout: X86Vcpu(0x88) + vcpu_id(u32@0x88) + launched(bool@0x8C)
// HOST_RSP VMCS field encoding = 0x6c14
// HOST_RIP VMCS field encoding = 0x6c16

.text
.globl vmx_enter_guest
.globl kvmm_vmx_return

// int vmx_enter_guest(vcpu_t *vcpu)
// rdi = pointer to X86Vcpu (start of Vcpu<X86Vmx>)
vmx_enter_guest:
    // 1. Save host callee-saved registers.
    push  %rbp
    push  %rbx
    push  %r12
    push  %r13
    push  %r14
    push  %r15
    push  %rdi                  // save vcpu* on stack

    // 2. Set HOST_RSP = current RSP (vmexit restores to here).
    movq  %rsp, %rax
    movq  $0x6c14, %rcx         // HOST_RSP encoding
    vmwrite %rax, %rcx

    // 3. Set HOST_RIP = kvmm_vmx_return.
    leaq  kvmm_vmx_return(%rip), %rax
    movq  $0x6c16, %rcx         // HOST_RIP encoding
    vmwrite %rax, %rcx

    // 4. Check launched flag (offset 0x8C from vcpu*) and save on stack.
    movzbl 0x8C(%rdi), %eax
    push  %rax                     // save launched flag on stack

    // 5. Load guest GPRs from vcpu->regs.
    movq  0x00(%rdi), %rax
    movq  0x08(%rdi), %rbx
    movq  0x10(%rdi), %rcx
    movq  0x18(%rdi), %rdx
    movq  0x20(%rdi), %rbp
    movq  0x38(%rdi), %r8
    movq  0x40(%rdi), %r9
    movq  0x48(%rdi), %r10
    movq  0x50(%rdi), %r11
    movq  0x58(%rdi), %r12
    movq  0x60(%rdi), %r13
    movq  0x68(%rdi), %r14
    movq  0x70(%rdi), %r15
    // Load rsi, rdi last (rdi holds vcpu* until now).
    movq  0x28(%rdi), %rsi
    movq  0x30(%rdi), %rdi      // rdi = guest rdi; vcpu* only on stack

    // 6. vmlaunch or vmresume.
    cmpb  $0, (%rsp)              // test launched flag (top of stack)
    jnz   .Lkvmm_do_resume
    vmlaunch
    jmp   .Lkvmm_entry_failed
.Lkvmm_do_resume:
    vmresume

.Lkvmm_entry_failed:
    // vmlaunch/vmresume failed: RSP unchanged, restore host state.
    add   $16, %rsp              // skip launched flag + saved rdi (vcpu*)
    pop   %r15
    pop   %r14
    pop   %r13
    pop   %r12
    pop   %rbx
    pop   %rbp
    xor   %eax, %eax            // return 0 = failure
    ret

kvmm_vmx_return:
    // VM-exit: CPU restored HOST_RSP/HOST_RIP/segments.
    // GPRs rax-r15 hold guest values. RSP = &saved_vcpu* on stack.
    // Save guest GPRs to vcpu->regs.
    xchg  %r15, (%rsp)          // r15 = vcpu*, (rsp) = guest r15

    movq  %rax, 0x00(%r15)
    movq  %rbx, 0x08(%r15)
    movq  %rcx, 0x10(%r15)
    movq  %rdx, 0x18(%r15)
    movq  %rbp, 0x20(%r15)
    movq  %rsi, 0x28(%r15)
    movq  %rdi, 0x30(%r15)
    movq  %r8,  0x38(%r15)
    movq  %r9,  0x40(%r15)
    movq  %r10, 0x48(%r15)
    movq  %r11, 0x50(%r15)
    movq  %r12, 0x58(%r15)
    movq  %r13, 0x60(%r15)
    movq  %r14, 0x68(%r15)
    movq  (%rsp), %rax          // rax = guest r15 (from xchg)
    movq  %rax, 0x70(%r15)

    // Mark vcpu as launched (offset 0x8C = launched flag).
    movb  $1, 0x8C(%r15)

    // Restore host callee-saved registers.
    add   $8, %rsp              // skip guest r15 slot
    pop   %r15
    pop   %r14
    pop   %r13
    pop   %r12
    pop   %rbx
    pop   %rbp

    mov   $1, %eax              // return 1 = success
    ret

.section .note.GNU-stack,"",@progbits
