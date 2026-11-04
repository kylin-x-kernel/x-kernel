// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

.section .text
.balign 4096
.global kvmm_timer_guest_start
kvmm_timer_guest_start:
    mov x22, x0
    mov x24, x1
    adr x0, .Ltimer_vectors
    msr vbar_el1, x0
    isb
    stp xzr, xzr, [x22]
    str xzr, [x22, #16]
    movz x21, #0x0801, lsl #16
    mov x0, #255
    str w0, [x21, #4]
    str wzr, [x21, #8]
    mov x0, #1
    str w0, [x21]
    movz x1, #0x0800, lsl #16
    str w0, [x1]
    mov x0, #(1 << 27)
    str w0, [x1, #0x100]
    mrs x23, cntfrq_el0
    mov x0, #100
    udiv x23, x23, x0
    mov x19, #0
    msr daifclr, #2
.Ltimer_arm:
    msr daifset, #2
    mrs x0, cntvct_el0
    add x0, x0, x23
    msr cntv_cval_el0, x0
    mov x0, #1
    msr cntv_ctl_el0, x0
    isb
    add x19, x19, #1
.Ltimer_due:
    mrs x0, cntvct_el0
    mrs x1, cntv_cval_el0
    cmp x0, x1
    b.hs .Ltimer_pending
    wfi
    b .Ltimer_due
.Ltimer_pending:
    // Withdraw an expired but unacknowledged level without migrating. The
    // host must retire its pending LR and physical active backing on exit.
    msr cntv_ctl_el0, xzr
    isb
    movz x1, #0x1000, lsl #16
    str w0, [x1, #24]
    mrs x0, cntvct_el0
    add x0, x0, x23
    msr cntv_cval_el0, x0
    mov x0, #1
    msr cntv_ctl_el0, x0
    isb
.Ltimer_rearmed_due:
    mrs x0, cntvct_el0
    mrs x1, cntv_cval_el0
    cmp x0, x1
    b.hs .Ltimer_migrate
    wfi
    b .Ltimer_rearmed_due
.Ltimer_migrate:
    movz x1, #0x1000, lsl #16
    str w0, [x1]
    msr daifclr, #2
.Ltimer_wait:
    ldr x0, [x22]
    cmp x0, x19
    b.hs .Ltimer_delivered
    wfi
    b .Ltimer_wait
.Ltimer_delivered:
    cmp x19, #8
    b.lo .Ltimer_arm
    mov x0, #0x600d
    str x0, [x22, #16]
    b .Ltimer_done
.Ltimer_fail:
    mov x0, #1
    str x0, [x22, #8]
.Ltimer_done:
    msr cntv_ctl_el0, xzr
    msr daifset, #2
    mov x0, #0
    hvc #0
    b .Ltimer_done

.balign 2048
.Ltimer_vectors:
    .rept 5
    b .Ltimer_fail
    .balign 128
    .endr
    b .Ltimer_irq
    .balign 128
    .rept 10
    b .Ltimer_fail
    .balign 128
    .endr
.Ltimer_irq:
    stp x0, x1, [sp, #-32]!
    str x2, [sp, #16]
    ldr w0, [x21, #12]
    and w1, w0, #0x3ff
    cmp w1, #27
    b.ne .Ltimer_fail
    movz x1, #0x1000, lsl #16
    str w0, [x1, #8]
    mrs x1, cntvct_el0
    mrs x2, cntv_cval_el0
    cmp x1, x2
    b.lo .Ltimer_fail
    // Guest 1 exits with its final acknowledged timer still active/asserted.
    // Final host teardown, not a guest EOI or disable, must revoke it.
    cbz x24, .Ltimer_lower
    cmp x19, #8
    b.ne .Ltimer_lower
    str x19, [x22]
    mov x0, #0x600d
    str x0, [x22, #16]
    msr daifset, #2
    mov x0, #0
    hvc #0
    b .Ltimer_fail
.Ltimer_lower:
    msr cntv_ctl_el0, xzr
    isb
    movz x1, #0x1000, lsl #16
    str w0, [x1, #16]
    str w0, [x21, #16]
    dsb sy
    ldr x1, [x22]
    add x1, x1, #1
    str x1, [x22]
    ldr x2, [sp, #16]
    ldp x0, x1, [sp], #32
    eret
.global kvmm_timer_guest_end
kvmm_timer_guest_end:
