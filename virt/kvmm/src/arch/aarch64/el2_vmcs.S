// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 VHE guest entry/exit assembly.
//
// el2_enter_guest(vcpu: *mut Aarch64Vcpu)
//   Saves host callee-saved regs, switches HCR_EL2 to guest mode,
//   installs guest VBAR, loads guest state, eret into EL1 guest.
//   Returns via el2_trap_exit when guest traps to EL2.
//
// el2_trap_exit
//   Saves guest state, restores HCR_EL2 to host mode,
//   restores host VBAR and callee-saved regs, ret.
//
// Aarch64Vcpu layout (offsets match Rust #[repr(C)] struct):
//   0   : gprs[31]    31×8 = 248 B
//   248 : sp_el1      8 B
//   256 : elr         8 B
//   264 : spsr        8 B
//   272 : host_ctx[13] 13×8 = 104 B
//   376 : host_vbar   8 B
//   384 : host_tpidr  8 B
//   392 : sysregs[128] 128 B

.arch armv8.2-a

// ── el2_enter_guest ──
.section .text
.global el2_enter_guest
.type el2_enter_guest, @function
el2_enter_guest:
    // 1. Save host per-CPU pointer (accessed via tpidr_el1 with TGE=1
    //    under VHE E2H=1 aliasing), then stash vcpu pointer in tpidr_el2
    //    (used by el2_trap_exit with TGE=0 to find vcpu).
    mrs  x1, tpidr_el1
    str  x1, [x0, #384]         // vcpu->host_tpidr
    msr  tpidr_el2, x0

    // 2. Save host callee-saved regs to vcpu->host_ctx (offset 272).
    add  x1, x0, #272
    stp  x19, x20, [x1, #0]
    stp  x21, x22, [x1, #16]
    stp  x23, x24, [x1, #32]
    stp  x25, x26, [x1, #48]
    stp  x27, x28, [x1, #64]
    stp  x29, x30, [x1, #80]
    mov  x2,  sp
    str  x2,       [x1, #96]

    // 3. Load guest sp_el1, elr_el2, spsr_el2.
    ldr  x2, [x0, #248]
    msr  sp_el1,  x2
    ldr  x2, [x0, #256]
    msr  elr_el2, x2
    ldr  x2, [x0, #264]
    msr  spsr_el2, x2
    isb

    // 4. Save host VBAR_EL2 (via vbar_el1 VHE alias), install guest VBAR.
    mrs  x2, vbar_el1
    str  x2, [x0, #376]         // vcpu->host_vbar

    // 5. Switch HCR_EL2 to guest mode: clear TGE, set TWI.
    //    VM bit is NOT set here — stage-2 must be configured separately
    //    via kvmm_set_stage2_pgd before enabling VM.
    mrs  x2, hcr_el2
    bic  x2, x2, #(1 << 27)    // clear TGE
    orr  x2, x2, #(1 << 13)    // set TWI
    msr  hcr_el2, x2
    isb

    // 6. Install guest_vec_table as VBAR_EL2.
    //    With TGE=0, vbar_el1 accesses the real EL1 register (guest's),
    //    so we must write vbar_el2 directly for the EL2 trap vector.
    adrp x2, kvmm_guest_vec_table
    add  x2, x2, :lo12:kvmm_guest_vec_table
    msr  vbar_el2, x2
    isb

    // 6b. Restore guest TPIDR_EL1 (with TGE=0, accesses real EL1 register).
    // sysregs[64] = vcpu offset 392+64 = 456.
    ldr  x2, [x0, #456]
    msr  tpidr_el1, x2

    // 7. Load guest GPRs x1-x30 (x0 last, it holds the vcpu pointer).
    ldr  x1,       [x0, #8]
    ldp  x2,  x3,  [x0, #16]
    ldp  x4,  x5,  [x0, #32]
    ldp  x6,  x7,  [x0, #48]
    ldp  x8,  x9,  [x0, #64]
    ldp  x10, x11, [x0, #80]
    ldp  x12, x13, [x0, #96]
    ldp  x14, x15, [x0, #112]
    ldp  x16, x17, [x0, #128]
    ldp  x18, x19, [x0, #144]
    ldp  x20, x21, [x0, #160]
    ldp  x22, x23, [x0, #176]
    ldp  x24, x25, [x0, #192]
    ldp  x26, x27, [x0, #208]
    ldp  x28, x29, [x0, #224]
    ldr  x30,      [x0, #240]
    ldr  x0,       [x0, #0]    // guest x0 last

    // 8. eret → EL1 guest.
    eret
.size el2_enter_guest, . - el2_enter_guest


// ── el2_trap_exit ──
// Called from guest_vec_table when guest traps to EL2.
// CPU state on entry: x0-x30 = guest values, ELR/SPSR_EL2 = guest PC/PSTATE,
// SP = host SP_EL2 (unchanged).
.global el2_trap_exit
.type el2_trap_exit, @function
el2_trap_exit:
    // Spill guest x0, x1 onto host stack.
    sub  sp, sp, #16
    stp  x0, x1, [sp, #0]

    // Recover vcpu pointer from tpidr_el2.
    mrs  x0, tpidr_el2

    // Save guest x2-x30.
    stp  x2,  x3,  [x0, #16]
    stp  x4,  x5,  [x0, #32]
    stp  x6,  x7,  [x0, #48]
    stp  x8,  x9,  [x0, #64]
    stp  x10, x11, [x0, #80]
    stp  x12, x13, [x0, #96]
    stp  x14, x15, [x0, #112]
    stp  x16, x17, [x0, #128]
    stp  x18, x19, [x0, #144]
    stp  x20, x21, [x0, #160]
    stp  x22, x23, [x0, #176]
    stp  x24, x25, [x0, #192]
    stp  x26, x27, [x0, #208]
    stp  x28, x29, [x0, #224]
    str  x30,      [x0, #240]

    // Restore guest x0, x1 from stack and save to vcpu.
    ldp  x1, x2, [sp, #0]
    add  sp, sp, #16
    str  x1, [x0, #0]          // vcpu->gprs[0] = guest x0
    str  x2, [x0, #8]          // vcpu->gprs[1] = guest x1

    // Save guest sp_el1, elr_el2, spsr_el2.
    mrs  x1, sp_el1
    str  x1, [x0, #248]
    mrs  x1, elr_el2
    str  x1, [x0, #256]
    mrs  x1, spsr_el2
    str  x1, [x0, #264]

    // Save guest TPIDR_EL1 while TGE=0 (accesses real EL1 register).
    // sysregs[64] = vcpu offset 392+64 = 456.
    mrs  x1, tpidr_el1
    str  x1, [x0, #456]

    // Restore HCR_EL2 to host mode: set TGE, clear TWI.
    mrs  x1, hcr_el2
    orr  x1, x1, #(1 << 27)    // set TGE
    bic  x1, x1, #(1 << 13)    // clear TWI
    msr  hcr_el2, x1
    isb

    // Restore host VBAR_EL2 (via vbar_el1 alias, since TGE=1 now).
    ldr  x1, [x0, #376]         // vcpu->host_vbar
    msr  vbar_el1, x1
    isb

    // Restore host per-CPU pointer (via tpidr_el1, which with TGE=1
    //    aliases to the register the kernel reads for per-CPU data).
    ldr  x1, [x0, #384]         // vcpu->host_tpidr
    msr  tpidr_el1, x1

    // Restore host callee-saved regs from vcpu->host_ctx (offset 272).
    add  x1, x0, #272
    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]
    ldr  x2,       [x1, #96]
    mov  sp, x2

    ret
.size el2_trap_exit, . - el2_trap_exit
