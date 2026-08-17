// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// RISC-V H-ext guest test program — finite version for selftest.
//
// Runs in VS-mode (vsatp=0, no translation).
//
// a0 = test mode on entry:
//   0 → normal: 100 iterations then MMIO + ecall(DONE)
//   non-zero → infinite WFI loop (VM stays alive)
//
// ecall protocol (a7 = hypercall number):
//   a7=0x584b0001 (PRINT): a0 = iteration count
//   a7=0x584b0000 (DONE):  guest finished

.equ GUEST_ECALL_DONE,   0x584b0000
.equ GUEST_ECALL_PRINT,  0x584b0001

.section .text
.balign 4
.global kvmm_guest_test_entry_rv
.type kvmm_guest_test_entry_rv, @function
kvmm_guest_test_entry_rv:
    // Check test mode: a0 != 0 → infinite loop
    bnez    a0, .Linfinite

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

    // Test MMIO: store to unmapped GPA triggers G-stage fault.
    // The VMM handles it via MmioBus dispatch and resumes.
    li      t0, 0x10000000
    sw      s0, 0(t0)

    // Done.
    li      a7, GUEST_ECALL_DONE
    li      a0, 0
    ecall

    // Unreachable.
1:  wfi
    j       1b

.Linfinite:
    //wfi
    j       .Linfinite

.global kvmm_guest_test_entry_rv_end
kvmm_guest_test_entry_rv_end:
.size kvmm_guest_test_entry_rv, . - kvmm_guest_test_entry_rv
