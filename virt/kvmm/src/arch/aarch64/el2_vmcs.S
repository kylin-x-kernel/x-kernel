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
//   520 : exit_type   8 B
//   528 : esr         8 B  (captured by guest vector)
//   536 : far         8 B  (captured on exit below)
//   544 : hpfar       8 B  (captured on exit below)
//   552 : cntv_ctl    8 B  (guest vtimer, saved/disabled on exit)
//   560 : cntv_cval   8 B  (guest vtimer, saved on exit)

.arch armv8.2-a

// ── el2_enter_guest ──
.section .text
.global el2_enter_guest
.type el2_enter_guest, @function
el2_enter_guest:
    // 0. Mask IRQ to prevent preemption between tpidr_el2 write and eret.
    //    With VHE (E2H=1), tpidr_el1 and tpidr_el2 alias the same physical
    //    register. If preempted, another vCPU thread could overwrite it.
    msr  daifset, #2            // PSTATE.I = 1 (mask IRQ)

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

    // 5. Switch HCR_EL2 to guest mode: clear TGE, set TSC+TWI+TWE+IMO+FMO.
    //    IMO/FMO route physical IRQ/FIQ to EL2 so the host timer can
    //    preempt a guest running a tight loop (no voluntary exit).
    //    VM bit (Stage-2 enable) is set by Stage2::activate() if configured.
    mrs  x2, hcr_el2
    bic  x2, x2, #(1 << 27)    // clear TGE
    orr  x2, x2, #(1 << 19)    // set TSC (trap SMC to EL2)
    orr  x2, x2, #(1 << 13)    // set TWI
    orr  x2, x2, #(1 << 14)    // set TWE
    orr  x2, x2, #(1 << 4)     // set IMO (route IRQ to EL2)
    orr  x2, x2, #(1 << 3)     // set FMO (route FIQ to EL2)
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

    // Restore guest SP_EL0 while TGE=0. Linux arm64 stores `current` here when
    // running in EL1h, so this must be per-vCPU or current-task state crosses
    // CPUs under SMP.
    ldr  x2, [x0, #480]
    msr  sp_el0, x2

    // 6c. Restore the guest virtual timer (saved + disabled on the previous
    //     exit). Restore CVAL before CTL so a stale compare cannot fire.
    ldr  x2, [x0, #560]         // vcpu->cntv_cval
    msr  S3_5_C14_C3_2, x2      // CNTV_CVAL_EL02
    ldr  x2, [x0, #552]         // vcpu->cntv_ctl
    msr  S3_5_C14_C3_1, x2      // CNTV_CTL_EL02
    isb

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


// ── el2_trap_exit_common ──
// Called from guest_vec_table when guest traps to EL2.
// On entry: x0 = vcpu* (from tpidr_el2), guest x0/x1 on stack,
// exit_type already stored to vcpu->exit_type by vector entry.
// x2-x30 = guest values.
.global el2_trap_exit_common
.type el2_trap_exit_common, @function
el2_trap_exit_common:
    // Save guest x2-x30 (x0 = vcpu*).
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

    // Pop guest x0, x1 from stack and save to vcpu.
    ldp  x1, x2, [sp], #16
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

    // Save guest SP_EL0 while TGE=0, before returning to host context. On
    // Linux/arm64 this is the per-CPU current task pointer in kernel mode.
    mrs  x1, sp_el0
    str  x1, [x0, #480]

    // Capture fault address registers before returning to host code. ESR_EL2
    // is captured by the guest vector; FAR_EL2/HPFAR_EL2 are captured here.
    // These EL2 exception registers are not vCPU-private and can be overwritten
    // by a later host exception once the host unmasks IRQs in Rust.
    mrs  x1, far_el2
    mrs  x2, hpfar_el2
    str  x1, [x0, #536]         // vcpu->far
    str  x2, [x0, #544]         // vcpu->hpfar

    // Save the guest virtual timer and DISABLE it, so its PPI 27 cannot assert
    // while the host runs (no "Unhandled IRQ 27" leak, no ticks counted during
    // host time). The Rust exit path re-injects the tick in software and the
    // next entry restores CTL/CVAL. Runs while IRQs are still masked.
    mrs  x1, S3_5_C14_C3_1      // CNTV_CTL_EL02
    mrs  x2, S3_5_C14_C3_2      // CNTV_CVAL_EL02
    str  x1, [x0, #552]         // vcpu->cntv_ctl
    str  x2, [x0, #560]         // vcpu->cntv_cval
    // msr  S3_5_C14_C3_1, xzr     // disable timer → PPI 27 deasserts
    isb

    // Restore host VBAR_EL2 before switching HCR back to host mode. While the
    // guest vector is still installed, any host-side EL2 exception would reuse
    // the guest exit path and stale tpidr_el2 — especially visible when
    // multiple vCPUs share one physical CPU.
    ldr  x1, [x0, #376]         // vcpu->host_vbar
    msr  vbar_el2, x1
    isb

    // Restore HCR_EL2 to host mode: set TGE, clear TSC+TWI+TWE+VM+IMO+FMO.
    mrs  x1, hcr_el2
    orr  x1, x1, #(1 << 27)    // set TGE
    bic  x1, x1, #(1 << 19)    // clear TSC
    bic  x1, x1, #(1 << 13)    // clear TWI
    bic  x1, x1, #(1 << 14)    // clear TWE
    bic  x1, x1, #(1 << 4)     // clear IMO
    bic  x1, x1, #(1 << 3)     // clear FMO
    bic  x1, x1, #1            // clear VM (disable Stage-2)
    msr  hcr_el2, x1
    isb

    // With VHE and TGE=1 host exceptions use the VBAR_EL1 alias. Restore it
    // after the HCR switch as well; the earlier VBAR_EL2 write only closes the
    // transient window while TGE was still clear.
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

    // Restore host uaccess state before returning to Rust. A guest exit can
    // leave privileged access to EL0 mappings disabled via PSTATE.PAN, while
    // the host copy_user path expects PAN to be clear.
    msr  pan, #0

    ret
.size el2_trap_exit_common, . - el2_trap_exit_common
