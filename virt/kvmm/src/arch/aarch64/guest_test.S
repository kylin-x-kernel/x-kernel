// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 VHE guest test program — finite version for selftest.
//
// Runs at EL1 (guest) under VHE (host at EL2 with E2H=1).
// Does 100 iterations of WFI + HVC_PRINT, then HVC_DONE.
//
// HVC protocol:
//   x0=1 (PRINT): x1 = iteration count
//   x0=0 (DONE):  guest finished

.section .text
.global kvmm_guest_test_entry
.type kvmm_guest_test_entry, @function
kvmm_guest_test_entry:
    mov  x19, #0              // iteration counter
    mov  x20, #100            // max iterations

.Lgt_loop:
    wfi

    mov  x0, #1              // HVC_PRINT
    mov  x1, x19             // iteration count
    hvc  #0

    add  x19, x19, #1
    cmp  x19, x20
    b.lt .Lgt_loop

    // Done: signal VMM to exit.
    mov  x0, #0              // HVC_DONE
    mov  x1, x20             // total iterations
    hvc  #0

    // Unreachable.
1:  wfe
    b    1b
.size kvmm_guest_test_entry, . - kvmm_guest_test_entry
