// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Resume through a firmware real-mode waking vector or an AP startup IPI.
// The platform copies only [s3_wake_start, s3_wake_end) into its reserved page.
.equ s3_gdtr_offset, s3_wake_gdtr - s3_wake_start
.equ s3_cr3_offset, s3_wake_cr3 - s3_wake_start
.equ s3_context_offset, s3_wake_context - s3_wake_start
.equ s3_target_offset, s3_wake_target - s3_wake_start

.equ MSR_EFER, 0xc0000080
.equ MSR_GS_BASE, 0xc0000101
.equ MSR_KERNEL_GS_BASE, 0xc0000102
.equ MSR_FS_BASE, 0xc0000100
.equ MSR_STAR, 0xc0000081
.equ MSR_LSTAR, 0xc0000082
.equ MSR_FMASK, 0xc0000084
.equ MSR_APIC_BASE, 0x1b

.section .text
.code16
.balign 16
.global s3_wake_start
s3_wake_start:
    cli
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax
    movzx ebx, ax
    shl ebx, 4
    lgdt [s3_gdtr_offset]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    .byte 0x66, 0xea
.global s3_wake_pm_patch
s3_wake_pm_patch:
    .long 0
    .word 8
.code32
.global s3_wake_pm
s3_wake_pm:
    mov ax, 0x18
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, 0x620 // CR4: PAE | OSFXSR | OSXMMEXCPT
    mov cr4, eax
    mov eax, [ebx + s3_cr3_offset]
    mov cr3, eax
    mov ecx, MSR_EFER
    mov eax, 0x901 // EFER: SCE | LME | NXE
    xor edx, edx
    wrmsr
    mov eax, 0x80010033 // CR0: PG | WP | NE | ET | MP | PE
    mov cr0, eax
    .byte 0xea
.global s3_wake_lm_patch
s3_wake_lm_patch:
    .long 0
    .word 0x10
.code64
.global s3_wake_lm
s3_wake_lm:
    lea rbx, [rip + s3_wake_start]
    mov rdi, [rbx + s3_context_offset]
    mov rax, [rbx + s3_target_offset]
    jmp rax
.balign 8
s3_wake_gdtr:
    .word s3_wake_gdt_end - s3_wake_gdt - 1
.global s3_wake_gdt_patch
s3_wake_gdt_patch:
    .long 0
.global s3_wake_gdt
s3_wake_gdt:
    .quad 0, 0x00cf9b000000ffff, 0x00af9b000000ffff, 0x00cf93000000ffff
s3_wake_gdt_end:
.global s3_wake_cr3
s3_wake_cr3:
    .quad 0
.global s3_wake_context
s3_wake_context:
    .quad 0
.global s3_wake_target
s3_wake_target:
    .quad 0
.global s3_wake_end
s3_wake_end:

.macro S3_SAVE_MSR number, offset
    mov ecx, \number
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rdi + \offset], rax
.endm
.macro S3_RESTORE_MSR number, offset
    mov ecx, \number
    mov rax, [rdi + \offset]
    mov rdx, rax
    shr rdx, 32
    wrmsr
.endm

.code64
.global s3_save_and_call
.type s3_save_and_call, @function
// (context, callback, argument): returns once, after callback abort or S3 wake.
s3_save_and_call:
    mov [rdi + {ctx_rsp}], rsp
    mov [rdi + {ctx_rbx}], rbx
    mov [rdi + {ctx_rbp}], rbp
    mov [rdi + {ctx_r12}], r12
    mov [rdi + {ctx_r13}], r13
    mov [rdi + {ctx_r14}], r14
    mov [rdi + {ctx_r15}], r15
    mov r9, rdx
    mov rax, cr3
    mov [rdi + {ctx_cr3}], rax
    mov rax, cr0
    mov [rdi + {ctx_cr0}], rax
    mov rax, cr4
    mov [rdi + {ctx_cr4}], rax
    S3_SAVE_MSR MSR_EFER, {ctx_efer}
    S3_SAVE_MSR MSR_GS_BASE, {ctx_gs_base}
    S3_SAVE_MSR MSR_KERNEL_GS_BASE, {ctx_kernel_gs_base}
    S3_SAVE_MSR MSR_FS_BASE, {ctx_fs_base}
    S3_SAVE_MSR MSR_STAR, {ctx_star}
    S3_SAVE_MSR MSR_LSTAR, {ctx_lstar}
    S3_SAVE_MSR MSR_FMASK, {ctx_fmask}
    S3_SAVE_MSR MSR_APIC_BASE, {ctx_apic_base}
    sgdt [rdi + {ctx_gdtr}]
    sidt [rdi + {ctx_idtr}]
    str word ptr [rdi + {ctx_tr}]
    mov rax, cr2
    mov [rdi + {ctx_cr2}], rax
    mov rax, cr8
    mov [rdi + {ctx_cr8}], rax
    fxsave64 [rdi + {ctx_fxsave}]
    wbinvd
    mov r12, rdi
    mov rdi, r9
    sub rsp, 8
    call rsi
    add rsp, 8
    mov rdi, r12
    jmp s3_restore_context
.size s3_save_and_call, .-s3_save_and_call

.global s3_restore_context
.type s3_restore_context, @function
s3_restore_context:
    // Both bootstrap and saved roots map this kernel-image text and context.
    mov rax, [rdi + {ctx_cr3}]
    mov cr3, rax
    mov rax, [rdi + {ctx_cr4}]
    mov cr4, rax
    mov rax, [rdi + {ctx_cr0}]
    mov cr0, rax
    S3_RESTORE_MSR MSR_EFER, {ctx_efer}
    lgdt [rdi + {ctx_gdtr}]
    lidt [rdi + {ctx_idtr}]
    mov ax, 0x10
    mov ss, ax
    mov ds, ax
    mov es, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax
    S3_RESTORE_MSR MSR_GS_BASE, {ctx_gs_base}
    S3_RESTORE_MSR MSR_KERNEL_GS_BASE, {ctx_kernel_gs_base}
    S3_RESTORE_MSR MSR_FS_BASE, {ctx_fs_base}
    S3_RESTORE_MSR MSR_STAR, {ctx_star}
    S3_RESTORE_MSR MSR_LSTAR, {ctx_lstar}
    S3_RESTORE_MSR MSR_FMASK, {ctx_fmask}
    S3_RESTORE_MSR MSR_APIC_BASE, {ctx_apic_base}
    movzx ecx, word ptr [rdi + {ctx_tr}]
    mov rax, [rdi + {ctx_gdtr} + 2] // descriptor-table base follows its 16-bit limit
    and byte ptr [rax + rcx + 5], 0xfd // clear busy in the TSS descriptor access byte before LTR
    ltr cx
    mov rax, [rdi + {ctx_cr2}]
    mov cr2, rax
    mov rax, [rdi + {ctx_cr8}]
    mov cr8, rax
    fxrstor64 [rdi + {ctx_fxsave}]
    mov rsp, [rdi + {ctx_rsp}]
    push 8
    lea rax, [rip + .Ls3_restored_cs]
    push rax
    retfq
.Ls3_restored_cs:
    mov rbx, [rdi + {ctx_rbx}]
    mov rbp, [rdi + {ctx_rbp}]
    mov r12, [rdi + {ctx_r12}]
    mov r13, [rdi + {ctx_r13}]
    mov r14, [rdi + {ctx_r14}]
    mov r15, [rdi + {ctx_r15}]
    ret
.size s3_restore_context, .-s3_restore_context
