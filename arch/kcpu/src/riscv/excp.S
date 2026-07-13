// This exception entry code uses conventional register save/restore sequences
// and standard trap prologue/epilogue patterns that are common in low-level
// operating system code for RISC-V. The load/store layout follows established
// architectural practice and is not specific to any single project.

.section .text
.balign 4
.global trap_vector_base
trap_vector_base:
    // sscratch == 0: trap from S mode
    // sscratch != 0: trap from U mode
    csrrw   sp, sscratch, sp    // swap sscratch and sp
    bnez    sp, .Ltrap_entry

    csrr    sp, sscratch        // put supervisor sp back
    addi    sp, sp, -{trapframe_size}

.Ltrap_entry:
    PUSH_GENERAL_REGS

    csrrw   t0, sscratch, zero  // save sscratch (sp) and zero it
    csrr    t1, sepc
    csrr    t2, sstatus
    STR     t0, sp, 2           // tf.regs.sp
    STR     t1, sp, 32          // tf.sepc
    STR     t2, sp, 33          // tf.sstatus

    andi    t2, t2, 1 << 8      // sstatus.SPP == 1
    beqz    t2, .Lexit_user

    mv      a0, sp
    la      ra, .Ltrap_return
    j       riscv_trap_handler

.Lexit_user:
    // Trapped from U-mode: gp currently holds the USER's gp. The kernel
    // percpu `gp` (per-hart, never carried in a trapframe) is restored from
    // the callee frame at slot-13 below. Stash the user gp into the
    // trapframe's slot-3 (UserContext.regs.gp) here -- before `LDR sp, sp, 0`
    // switches `sp` to the callee frame -- so `.Ltrap_return` can reload it on
    // return to U-mode. PUSH_POP_GENERAL_REGS no longer touches gp, so this is
    // the sole write of slot-3 on the U-mode entry path.
    STR     gp, sp, 3
    LDR     sp, sp, 0
    LDR     s0, sp, 0
    LDR     s1, sp, 1
    LDR     s2, sp, 2
    LDR     s3, sp, 3
    LDR     s4, sp, 4
    LDR     s5, sp, 5
    LDR     s6, sp, 6
    LDR     s7, sp, 7
    LDR     s8, sp, 8
    LDR     s9, sp, 9
    LDR     s10, sp, 10
    LDR     s11, sp, 11
    LDR     ra, sp, 12
    LDR     gp, sp, 13
    LDR     tp, sp, 14
    addi    sp, sp, 16 * XLENB
    ret

.global enter_user
enter_user:
    addi    sp, sp, -16 * XLENB
    STR     s0, sp, 0
    STR     s1, sp, 1
    STR     s2, sp, 2
    STR     s3, sp, 3
    STR     s4, sp, 4
    STR     s5, sp, 5
    STR     s6, sp, 6
    STR     s7, sp, 7
    STR     s8, sp, 8
    STR     s9, sp, 9
    STR     s10, sp, 10
    STR     s11, sp, 11
    STR     ra, sp, 12
    STR     gp, sp, 13
    STR     tp, sp, 14

    STR     sp, a0, 0
    mv      sp, a0
    csrw    sscratch, sp

.Ltrap_return:
    LDR     t0, sp, 32
    LDR     t1, sp, 33
    csrw    sepc, t0
    csrw    sstatus, t1
    // `gp` is this hart's per-CPU base -- per-hart-invariant, so it is never
    // restored from the (per-task, migratable) trapframe. For an S-mode trap
    // return (SPP==1) leave `gp` as this hart's own base. For a U-mode return
    // (SPP==0) restore the user's gp from slot-3 (saved by `.Lexit_user` on
    // entry). t0/t1 are caller-saved and restored by POP_GENERAL_REGS below.
    andi    t0, t1, 1 << 8          // SPP (from saved sstatus in t1)
    bnez    t0, .Lgp_skip           // S-mode: keep this hart's gp
    LDR     gp, sp, 3               // U-mode: restore user gp
.Lgp_skip:
    POP_GENERAL_REGS
    LDR     sp, sp, 2           // restore sp

    sret
