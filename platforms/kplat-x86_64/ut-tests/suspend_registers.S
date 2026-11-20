// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

.section .text
.code64
.balign 16
.global s3_test_clobbered_register_roundtrip
.type s3_test_clobbered_register_roundtrip, @function
// C ABI wrapper: retain the Rust caller's registers, seed all six saved
// registers, and observe the real restore path before restoring the caller.
s3_test_clobbered_register_roundtrip:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    sub rsp, 8
    mov rbx, 0x123456789abc0000
    mov rbp, 0x123456789abc0001
    mov r12, 0x123456789abc0002
    mov r13, 0x123456789abc0003
    mov r14, 0x123456789abc0004
    mov r15, 0x123456789abc0005
    mov rdx, rdi
    lea rsi, [rip + .Ls3_test_clobber_and_restore]
    call s3_save_and_call
    xor eax, eax
    mov rcx, 0x123456789abc0000
    cmp rbx, rcx
    je .Ls3_test_register_0_ok
    or eax, 1
.Ls3_test_register_0_ok:
    mov rcx, 0x123456789abc0001
    cmp rbp, rcx
    je .Ls3_test_register_1_ok
    or eax, 2
.Ls3_test_register_1_ok:
    mov rcx, 0x123456789abc0002
    cmp r12, rcx
    je .Ls3_test_register_2_ok
    or eax, 4
.Ls3_test_register_2_ok:
    mov rcx, 0x123456789abc0003
    cmp r13, rcx
    je .Ls3_test_register_3_ok
    or eax, 8
.Ls3_test_register_3_ok:
    mov rcx, 0x123456789abc0004
    cmp r14, rcx
    je .Ls3_test_register_4_ok
    or eax, 16
.Ls3_test_register_4_ok:
    mov rcx, 0x123456789abc0005
    cmp r15, rcx
    je .Ls3_test_register_5_ok
    or eax, 32
.Ls3_test_register_5_ok:
    add rsp, 8
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
.size s3_test_clobbered_register_roundtrip, .-s3_test_clobbered_register_roundtrip

// This callback never returns through the C ABI. It models lost firmware
// register state and transfers directly to the production restoration entry.
// rdi still names the live SavedCpu; no Rust frame sees these clobbered values.
.Ls3_test_clobber_and_restore:
    mov rbx, 0xfedcba9876540000
    mov rbp, 0xfedcba9876540001
    mov r12, 0xfedcba9876540002
    mov r13, 0xfedcba9876540003
    mov r14, 0xfedcba9876540004
    mov r15, 0xfedcba9876540005
    jmp s3_restore_context
