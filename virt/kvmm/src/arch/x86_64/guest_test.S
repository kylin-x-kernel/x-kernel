// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// x86_64 VMX guest test program — finite version for selftest.
//
// Runs in VMX non-root mode sharing host CR3 (identity-mapped).
// Does 100 iterations of HLT + VMCALL(PRINT) every 20, then VMCALL(DONE).
//
// Hypercall convention (matches x-kernel exit handler):
//   rax = hypercall number (0=DONE, 1=PRINT)
//   rdi = argument (iteration count)

.text
.balign 16
.globl kvmm_guest_test_entry_x86
.type kvmm_guest_test_entry_x86, @function
kvmm_guest_test_entry_x86:
    xorl    %ebx, %ebx          // iteration counter = 0

.Lgt_loop:
    hlt                          // VM-exit (HLT-exiting enabled)
    incl    %ebx

    // Check ebx % 20 == 0 via div.
    movl    %ebx, %eax
    movl    $20, %ecx
    xorl    %edx, %edx
    divl    %ecx                 // eax = ebx/20, edx = ebx%20
    testl   %edx, %edx
    jnz     .Lgt_skip_print

    // VMCALL_PRINT: rax=1, rdi=counter
    movq    $1, %rax
    movq    %rbx, %rdi
    vmcall

.Lgt_skip_print:
    cmpl    $100, %ebx
    jl      .Lgt_loop

    // VMCALL_DONE: rax=0, rdi=counter
    xorq    %rax, %rax
    movq    %rbx, %rdi
    vmcall

    // Unreachable.
1:  hlt
    jmp     1b
.size kvmm_guest_test_entry_x86, . - kvmm_guest_test_entry_x86

.section .note.GNU-stack,"",@progbits
