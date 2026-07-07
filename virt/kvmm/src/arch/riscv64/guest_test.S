// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// RISC-V H-ext guest test program — finite version for selftest.
//
// Runs in VS-mode (vsatp=0, no translation).
// Does 100 iterations of WFI + ecall(PRINT) every 20, then ecall(DONE).
//
// ecall protocol (a7 = hypercall number):
//   a7=1 (PRINT): a0 = iteration count
//   a7=0 (DONE):  guest finished

.equ GUEST_ECALL_DONE,   0
.equ GUEST_ECALL_PRINT,  1

.section .text
.balign 4
.global kvmm_guest_test_entry_rv
.type kvmm_guest_test_entry_rv, @function
kvmm_guest_test_entry_rv:
    li      s0, 0             // iteration counter
    li      s1, 100           // max iterations
    li      s2, 20            // print interval

.Lgt_loop:
    wfi

    addi    s0, s0, 1

    // ecall(PRINT) every 20 iterations (check via repeated subtraction).
    mv      t0, s0
.Lgt_mod:
    blt     t0, s2, .Lgt_mod_done
    sub     t0, t0, s2
    j       .Lgt_mod
.Lgt_mod_done:
    bnez    t0, .Lgt_skip_print

    li      a7, GUEST_ECALL_PRINT
    mv      a0, s0
    ecall

.Lgt_skip_print:
    blt     s0, s1, .Lgt_loop

    // Done.
    li      a7, GUEST_ECALL_DONE
    li      a0, 0
    ecall

    // Unreachable.
1:  wfi
    j       1b
.size kvmm_guest_test_entry_rv, . - kvmm_guest_test_entry_rv
