// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// SMCCC HVC/SMC helpers with exception-table fixups. On platforms where the
// conduit is unavailable from the current EL (e.g. SMC from EL1 on QEMU virt),
// the trap is redirected and NOT_SUPPORTED is returned instead of panicking.

.macro _asm_extable, from, to
    .pushsection __ex_table, "a"
    .balign 8
    .quad \from
    .quad \to
    .popsection
.endm

.section .text

// Args: x0=func, x1=arg0, x2=arg1, x3=arg2, x4=*SmcccResult out
// Stores x0..x3 into out after the call.
.global aarch64_peripherals_smccc_hvc_call
.type aarch64_peripherals_smccc_hvc_call, %function
aarch64_peripherals_smccc_hvc_call:
    mov x8, x4
1:  hvc #0
    stp x0, x1, [x8]
    stp x2, x3, [x8, #16]
    ret

.global aarch64_peripherals_smccc_smc_call
.type aarch64_peripherals_smccc_smc_call, %function
aarch64_peripherals_smccc_smc_call:
    mov x8, x4
2:  smc #0
    stp x0, x1, [x8]
    stp x2, x3, [x8, #16]
    ret

.Lhvc_fault:
    mov x0, xzr
    mvn x0, x0
    stp x0, xzr, [x8]
    stp xzr, xzr, [x8, #16]
    ret

.Lsmc_fault:
    mov x0, xzr
    mvn x0, x0
    stp x0, xzr, [x8]
    stp xzr, xzr, [x8, #16]
    ret

    _asm_extable 1b, .Lhvc_fault
    _asm_extable 2b, .Lsmc_fault
