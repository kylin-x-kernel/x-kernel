// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// RISC-V H-extension VM entry/exit assembly.
//
// hext_enter_guest(vcpu: *mut RiscvVcpu) -> i32
//   Saves host callee-saved regs, restores VS-CSRs, installs
//   hext_trap_vector, sets hstatus.SPV+SPVP, loads guest GPRs, sret.
//   Returns 1 via hext_trap_vector when guest traps to HS-mode.
//
// hext_trap_vector
//   Saves guest GPRs + VS-CSRs + scause/stval/htval, clears
//   hstatus.SPV+SPVP, restores host state, returns 1.
//
// RiscvVcpu layout (offsets match Rust #[repr(C)] struct):
//    0   : gprs[32]       32×8 = 256 B
//  256   : vsepc          8 B
//  264   : vsstatus       8 B
//  272   : vstvec         8 B
//  280   : vsscratch      8 B
//  288   : vsatp          8 B
//  296   : vsie           8 B
//  304   : scause_save    8 B
//  312   : stval_save     8 B
//  320   : htval_save     8 B
//  328   : host_ctx[16]   16×8 = 128 B

// vcpu_t field offsets
.equ VCPU_R0,         0
.equ VCPU_VSEPC,      256
.equ VCPU_VSSTATUS,   264
.equ VCPU_VSTVEC,     272
.equ VCPU_VSSCRATCH,  280
.equ VCPU_VSATP,      288
.equ VCPU_VSIE,       296
.equ VCPU_SCAUSE,     304
.equ VCPU_STVAL,      312
.equ VCPU_HTVAL,      320

// host_ctx offsets (VCPU_HOSTCTX = 328)
.equ HCTX_RA,         328
.equ HCTX_S0,         336
.equ HCTX_S1,         344
.equ HCTX_S2,         352
.equ HCTX_S3,         360
.equ HCTX_S4,         368
.equ HCTX_S5,         376
.equ HCTX_S6,         384
.equ HCTX_S7,         392
.equ HCTX_S8,         400
.equ HCTX_S9,         408
.equ HCTX_S10,        416
.equ HCTX_S11,        424
.equ HCTX_SP,         432
.equ HCTX_STVEC,      440
.equ HCTX_GP,         448

// H-extension CSR numbers (assembler may not recognize symbolic names)
.equ CSR_HSTATUS,     0x600
.equ CSR_VSSTATUS,    0x200
.equ CSR_VSIE,        0x204
.equ CSR_VSTVEC,      0x205
.equ CSR_VSSCRATCH,   0x240
.equ CSR_VSEPC,       0x241
.equ CSR_VSATP,       0x280
.equ CSR_HTVAL,       0x643

.section .text
.balign 4

// ── hext_enter_guest(vcpu: *mut RiscvVcpu) -> i32 ──
.global hext_enter_guest
.type hext_enter_guest, @function
hext_enter_guest:
    // 1. Save host callee-saved + sp + ra to vcpu->host_ctx.
    sd      ra,  HCTX_RA(a0)
    sd      s0,  HCTX_S0(a0)
    sd      s1,  HCTX_S1(a0)
    sd      s2,  HCTX_S2(a0)
    sd      s3,  HCTX_S3(a0)
    sd      s4,  HCTX_S4(a0)
    sd      s5,  HCTX_S5(a0)
    sd      s6,  HCTX_S6(a0)
    sd      s7,  HCTX_S7(a0)
    sd      s8,  HCTX_S8(a0)
    sd      s9,  HCTX_S9(a0)
    sd      s10, HCTX_S10(a0)
    sd      s11, HCTX_S11(a0)
    sd      sp,  HCTX_SP(a0)
    sd      gp,  HCTX_GP(a0)

    // 2. Save current stvec.
    csrr    t0, stvec
    sd      t0, HCTX_STVEC(a0)

    // 3. Restore VS-CSRs from vcpu.
    ld      t0, VCPU_VSEPC(a0)
    csrw    CSR_VSEPC, t0

    ld      t0, VCPU_VSSTATUS(a0)
    csrw    CSR_VSSTATUS, t0

    ld      t0, VCPU_VSTVEC(a0)
    csrw    CSR_VSTVEC, t0

    ld      t0, VCPU_VSSCRATCH(a0)
    csrw    CSR_VSSCRATCH, t0

    ld      t0, VCPU_VSATP(a0)
    csrw    CSR_VSATP, t0

    ld      t0, VCPU_VSIE(a0)
    csrw    CSR_VSIE, t0

    // 4. Install VMM trap vector.
    la      t0, kvmm_hext_trap_vector
    csrw    stvec, t0

    // 5. Store vcpu* in sscratch (trap handler retrieves it).
    csrw    sscratch, a0

    // 6. Set sepc = vcpu->vsepc (sret target).
    ld      t0, VCPU_VSEPC(a0)
    csrw    sepc, t0

    // 7. Set sstatus.SPP=1 (sret returns to S-mode = VS-mode), clear SPIE.
    csrr    t0, sstatus
    ori     t0, t0, 0x100       // SPP = bit8 = 1
    li      t1, ~0x20
    and     t0, t0, t1          // SPIE = bit5 = 0
    csrw    sstatus, t0

    // 8. Set hstatus.SPV=1 + SPVP=1 (sret enters VS-mode).
    csrr    t0, CSR_HSTATUS
    ori     t0, t0, 0x180       // SPV=bit7=1, SPVP=bit8=1
    csrw    CSR_HSTATUS, t0

    // 9. Load guest GPRs (a0 = x10 last).
    ld      x1,  VCPU_R0 +  1*8(a0)
    ld      x2,  VCPU_R0 +  2*8(a0)
    ld      x3,  VCPU_R0 +  3*8(a0)
    ld      x4,  VCPU_R0 +  4*8(a0)
    ld      x5,  VCPU_R0 +  5*8(a0)
    ld      x6,  VCPU_R0 +  6*8(a0)
    ld      x7,  VCPU_R0 +  7*8(a0)
    ld      x8,  VCPU_R0 +  8*8(a0)
    ld      x9,  VCPU_R0 +  9*8(a0)
    ld      x11, VCPU_R0 + 11*8(a0)
    ld      x12, VCPU_R0 + 12*8(a0)
    ld      x13, VCPU_R0 + 13*8(a0)
    ld      x14, VCPU_R0 + 14*8(a0)
    ld      x15, VCPU_R0 + 15*8(a0)
    ld      x16, VCPU_R0 + 16*8(a0)
    ld      x17, VCPU_R0 + 17*8(a0)
    ld      x18, VCPU_R0 + 18*8(a0)
    ld      x19, VCPU_R0 + 19*8(a0)
    ld      x20, VCPU_R0 + 20*8(a0)
    ld      x21, VCPU_R0 + 21*8(a0)
    ld      x22, VCPU_R0 + 22*8(a0)
    ld      x23, VCPU_R0 + 23*8(a0)
    ld      x24, VCPU_R0 + 24*8(a0)
    ld      x25, VCPU_R0 + 25*8(a0)
    ld      x26, VCPU_R0 + 26*8(a0)
    ld      x27, VCPU_R0 + 27*8(a0)
    ld      x28, VCPU_R0 + 28*8(a0)
    ld      x29, VCPU_R0 + 29*8(a0)
    ld      x30, VCPU_R0 + 30*8(a0)
    ld      x31, VCPU_R0 + 31*8(a0)
    ld      x10, VCPU_R0 + 10*8(a0)   // a0 last

    // 10. sret → VS-mode
    sret
.size hext_enter_guest, . - hext_enter_guest


// ── hext_trap_vector ──
// Guest trap handler. sscratch holds vcpu* on entry.
// Protocol:
//   csrrw a0, sscratch, a0  → a0=vcpu*, sscratch=guest_a0
//   csrr  t0, sscratch      → t0=guest_a0
//   csrw  sscratch, x0      → clear (kernel convention)
.global kvmm_hext_trap_vector
.type kvmm_hext_trap_vector, @function
.balign 4
kvmm_hext_trap_vector:
    // Atomically swap: a0=vcpu*, sscratch=guest_a0
    csrrw   a0, sscratch, a0

    // Read guest_a0 before clearing sscratch.
    csrr    t0, sscratch

    // Clear sscratch (kernel convention: sscratch=0 in HS-mode).
    csrw    sscratch, x0

    // 1. Save guest GPRs → vcpu->gprs[].
    sd      x1,  VCPU_R0 +  1*8(a0)
    sd      x2,  VCPU_R0 +  2*8(a0)
    sd      x3,  VCPU_R0 +  3*8(a0)
    sd      x4,  VCPU_R0 +  4*8(a0)
    sd      x5,  VCPU_R0 +  5*8(a0)
    sd      x6,  VCPU_R0 +  6*8(a0)
    sd      x7,  VCPU_R0 +  7*8(a0)
    sd      x8,  VCPU_R0 +  8*8(a0)
    sd      x9,  VCPU_R0 +  9*8(a0)
    sd      t0,  VCPU_R0 + 10*8(a0)  // guest a0 (from t0)
    sd      x11, VCPU_R0 + 11*8(a0)
    sd      x12, VCPU_R0 + 12*8(a0)
    sd      x13, VCPU_R0 + 13*8(a0)
    sd      x14, VCPU_R0 + 14*8(a0)
    sd      x15, VCPU_R0 + 15*8(a0)
    sd      x16, VCPU_R0 + 16*8(a0)
    sd      x17, VCPU_R0 + 17*8(a0)
    sd      x18, VCPU_R0 + 18*8(a0)
    sd      x19, VCPU_R0 + 19*8(a0)
    sd      x20, VCPU_R0 + 20*8(a0)
    sd      x21, VCPU_R0 + 21*8(a0)
    sd      x22, VCPU_R0 + 22*8(a0)
    sd      x23, VCPU_R0 + 23*8(a0)
    sd      x24, VCPU_R0 + 24*8(a0)
    sd      x25, VCPU_R0 + 25*8(a0)
    sd      x26, VCPU_R0 + 26*8(a0)
    sd      x27, VCPU_R0 + 27*8(a0)
    sd      x28, VCPU_R0 + 28*8(a0)
    sd      x29, VCPU_R0 + 29*8(a0)
    sd      x30, VCPU_R0 + 30*8(a0)
    sd      x31, VCPU_R0 + 31*8(a0)

    // 2. Save VS-CSRs → vcpu.
    csrr    t0, CSR_VSEPC
    sd      t0, VCPU_VSEPC(a0)

    csrr    t0, CSR_VSSTATUS
    sd      t0, VCPU_VSSTATUS(a0)

    csrr    t0, CSR_VSTVEC
    sd      t0, VCPU_VSTVEC(a0)

    csrr    t0, CSR_VSSCRATCH
    sd      t0, VCPU_VSSCRATCH(a0)

    csrr    t0, CSR_VSATP
    sd      t0, VCPU_VSATP(a0)

    csrr    t0, CSR_VSIE
    sd      t0, VCPU_VSIE(a0)

    // 3. Save HS trap info (sepc = guest PC on trap).
    csrr    t0, sepc
    sd      t0, VCPU_VSEPC(a0)    // overwrite vsepc with actual trap PC

    csrr    t0, scause
    sd      t0, VCPU_SCAUSE(a0)

    csrr    t0, stval
    sd      t0, VCPU_STVAL(a0)

    csrr    t0, CSR_HTVAL
    sd      t0, VCPU_HTVAL(a0)

    // 3b. Clear hstatus.SPV(bit7)+SPVP(bit8).
    // Without this, any subsequent sret in host context would
    // incorrectly enter VS/VU-mode.
    csrr    t0, CSR_HSTATUS
    li      t1, ~0x180
    and     t0, t0, t1
    csrw    CSR_HSTATUS, t0

    // 4. Restore original stvec.
    ld      t0, HCTX_STVEC(a0)
    csrw    stvec, t0

    // 5. Restore host callee-saved + sp.
    ld      ra,  HCTX_RA(a0)
    ld      s0,  HCTX_S0(a0)
    ld      s1,  HCTX_S1(a0)
    ld      s2,  HCTX_S2(a0)
    ld      s3,  HCTX_S3(a0)
    ld      s4,  HCTX_S4(a0)
    ld      s5,  HCTX_S5(a0)
    ld      s6,  HCTX_S6(a0)
    ld      s7,  HCTX_S7(a0)
    ld      s8,  HCTX_S8(a0)
    ld      s9,  HCTX_S9(a0)
    ld      s10, HCTX_S10(a0)
    ld      s11, HCTX_S11(a0)
    ld      sp,  HCTX_SP(a0)

    // 5b. Restore host gp (guest may have clobbered x3).
    ld      gp,  HCTX_GP(a0)

    // 6. Return 1 (successful guest exit).
    li      a0, 1
    ret
.size kvmm_hext_trap_vector, . - kvmm_hext_trap_vector
