// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 VHE guest EL1 system register save/restore.
//
// VHE (E2H=1): accessing EL1 sysregs from EL2 without _el12 suffix
// returns the EL2 aliased register (host). The _el12 suffix bypasses
// the E2H alias and accesses the real EL1 register (guest).
//
// Buffer layout (128 bytes):
//    0: sctlr_el12,  ttbr0_el12
//   16: ttbr1_el12,  tcr_el12
//   32: mair_el12,   amair_el12
//   48: vbar_el12,   cpacr_el12
//   64: tpidr_el1,   tpidr_el0   (not aliased by E2H)
//   80: tpidrro_el0, sp_el0
//   96: elr_el12,    spsr_el12
//  112: (padding)

.arch armv8.2-a

// ── save_sysregs_el12 ──
// @x0 = buffer pointer (>= 128 bytes)
.section .text
.global kvmm_save_sysregs_el12
.type kvmm_save_sysregs_el12, @function
kvmm_save_sysregs_el12:
    dsb ish
    isb

    mrs  x1, sctlr_el12
    mrs  x2, ttbr0_el12
    stp  x1, x2, [x0], #16

    mrs  x1, ttbr1_el12
    mrs  x2, tcr_el12
    stp  x1, x2, [x0], #16

    mrs  x1, mair_el12
    mrs  x2, amair_el12
    stp  x1, x2, [x0], #16

    mrs  x1, vbar_el12
    mrs  x2, cpacr_el12
    stp  x1, x2, [x0], #16

    // tpidr_el1 is aliased to tpidr_el2 under VHE (E2H=1, TGE=1).
    // The guest value is saved/restored in el2_vmcs.S where TGE=0.
    // Preserve the existing buffer value (written by el2_trap_exit).
    ldr  x1, [x0]
    mrs  x2, tpidr_el0
    stp  x1, x2, [x0], #16

    mrs  x1, tpidrro_el0
    mrs  x2, sp_el0
    stp  x1, x2, [x0], #16

    mrs  x1, elr_el12
    mrs  x2, spsr_el12
    stp  x1, x2, [x0], #16

    stp  xzr, xzr, [x0], #16

    ret
.size kvmm_save_sysregs_el12, . - kvmm_save_sysregs_el12


// ── restore_sysregs_el12 ──
// @x0 = buffer pointer (>= 128 bytes)
.global kvmm_restore_sysregs_el12
.type kvmm_restore_sysregs_el12, @function
kvmm_restore_sysregs_el12:
    ldp  x1, x2, [x0], #16
    msr  sctlr_el12, x1
    msr  ttbr0_el12, x2

    ldp  x1, x2, [x0], #16
    msr  ttbr1_el12, x1
    msr  tcr_el12, x2

    ldp  x1, x2, [x0], #16
    msr  mair_el12, x1
    msr  amair_el12, x2

    ldp  x1, x2, [x0], #16
    msr  vbar_el12, x1
    msr  cpacr_el12, x2

    ldp  x1, x2, [x0], #16
    // skip msr tpidr_el1 — aliased to tpidr_el2 under VHE+TGE=1.
    // Guest tpidr_el1 is restored in el2_vmcs.S where TGE=0.
    msr  tpidr_el0, x2

    ldp  x1, x2, [x0], #16
    msr  tpidrro_el0, x1
    msr  sp_el0, x2

    ldp  x1, x2, [x0], #16
    msr  elr_el12, x1
    msr  spsr_el12, x2

    dsb ish
    isb
    ret
.size kvmm_restore_sysregs_el12, . - kvmm_restore_sysregs_el12


// ── set_stage2_pgd ──
// @x0 = root page table physical address
// @x1 = VMID (8-bit)
.global kvmm_set_stage2_pgd
.type kvmm_set_stage2_pgd, @function
kvmm_set_stage2_pgd:
    and  x1, x1, #0xff
    lsl  x1, x1, #48
    orr  x0, x0, x1
    msr  vttbr_el2, x0
    dsb  ish
    isb
    ret
.size kvmm_set_stage2_pgd, . - kvmm_set_stage2_pgd
