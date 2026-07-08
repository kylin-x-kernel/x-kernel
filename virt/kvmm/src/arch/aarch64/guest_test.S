// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 VHE guest test program — finite version for selftest.
//
// Runs at EL1 (guest) under VHE (host at EL2 with E2H=1).
//
// x0 = test mode on entry:
//   0 → normal: 100 iterations then MMIO + HVC_DONE
//   non-zero → infinite WFI loop (VM stays alive)
//
// HVC protocol:
//   x0=1 (PRINT): x1 = iteration count
//   x0=0 (DONE):  guest finished

.section .text
.global kvmm_guest_test_entry
.type kvmm_guest_test_entry, @function
kvmm_guest_test_entry:
    // Check test mode: x0 != 0 → infinite loop
    cbnz x0, .Linfinite

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

    // Test MMIO: store to unmapped GPA triggers data abort.
    // The VMM handles it via MmioBus dispatch and resumes.
    movz x1, #0x1000, lsl #16
    str  x19, [x1]

    // Done: signal VMM to exit.
    mov  x0, #0              // HVC_DONE
    mov  x1, x20             // total iterations
    hvc  #0

    // Unreachable.
1:  wfe
    b    1b

.Linfinite:
    //wfi
    b    .Linfinite

.global kvmm_guest_test_entry_end
kvmm_guest_test_entry_end:
.size kvmm_guest_test_entry, . - kvmm_guest_test_entry
