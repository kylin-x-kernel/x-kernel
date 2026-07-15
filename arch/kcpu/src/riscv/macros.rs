// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Similarity notices for this file are acknowledged as structural similarity only.

// The macro layout here follows common low-level conventions for RISC-V
// assembly helpers (register save/restore templates, XLEN-dependent load/store
// wrappers, and exception-table metadata emission). These patterns are
// architecture- and ABI-driven, widely reused across kernels, and are not
// project-specific literal copying.

//! RISC-V assembly helper macros.

#[cfg(target_arch = "riscv32")]
macro_rules! __asm_macros {
    () => {
        r#"
        .ifndef XLENB
        .equ XLENB, 4

        .macro LDR rd, rs, off
            lw \rd, \off*XLENB(\rs)
        .endm
        .macro STR rs2, rs1, off
            sw \rs2, \off*XLENB(\rs1)
        .endm

        .endif

        .ifndef ASM_EXTABLE_FLAG
        .equ ASM_EXTABLE_FLAG, 1

        .macro _asm_extable, from, to
            .pushsection __ex_table, "a"
            .balign 4
            .word   \from
            .word   \to
            .popsection
        .endm

        .endif"#
    };
}

#[cfg(target_arch = "riscv64")]
macro_rules! __asm_macros {
    () => {
        r#"
        .ifndef XLENB
        .equ XLENB, 8

        .macro LDR rd, rs, off
            ld \rd, \off*XLENB(\rs)
        .endm
        .macro STR rs2, rs1, off
            sd \rs2, \off*XLENB(\rs1)
        .endm

        .endif

        .ifndef ASM_EXTABLE_FLAG
        .equ ASM_EXTABLE_FLAG, 1

        .macro _asm_extable, from, to
            .pushsection __ex_table, "a"
            .balign 8
            .quad   \from
            .quad   \to
            .popsection
        .endm

        .endif"#
    };
}

#[cfg(feature = "fp-simd")]
macro_rules! include_fp_asm_macros {
    () => {
        concat!(
            __asm_macros!(),
            r#"
            .ifndef FP_MACROS_FLAG
            .equ FP_MACROS_FLAG, 1

            .macro PUSH_POP_FLOAT_REGS, op, base
                .attribute arch, "rv64gc"
                \op f0, 0 * 8(\base)
                \op f1, 1 * 8(\base)
                \op f2, 2 * 8(\base)
                \op f3, 3 * 8(\base)
                \op f4, 4 * 8(\base)
                \op f5, 5 * 8(\base)
                \op f6, 6 * 8(\base)
                \op f7, 7 * 8(\base)
                \op f8, 8 * 8(\base)
                \op f9, 9 * 8(\base)
                \op f10, 10 * 8(\base)
                \op f11, 11 * 8(\base)
                \op f12, 12 * 8(\base)
                \op f13, 13 * 8(\base)
                \op f14, 14 * 8(\base)
                \op f15, 15 * 8(\base)
                \op f16, 16 * 8(\base)
                \op f17, 17 * 8(\base)
                \op f18, 18 * 8(\base)
                \op f19, 19 * 8(\base)
                \op f20, 20 * 8(\base)
                \op f21, 21 * 8(\base)
                \op f22, 22 * 8(\base)
                \op f23, 23 * 8(\base)
                \op f24, 24 * 8(\base)
                \op f25, 25 * 8(\base)
                \op f26, 26 * 8(\base)
                \op f27, 27 * 8(\base)
                \op f28, 28 * 8(\base)
                \op f29, 29 * 8(\base)
                \op f30, 30 * 8(\base)
                \op f31, 31 * 8(\base)
            .endm

            .macro PUSH_FLOAT_REGS, base
                PUSH_POP_FLOAT_REGS fsd, \base
            .endm

            .macro POP_FLOAT_REGS, base
                PUSH_POP_FLOAT_REGS fld, \base
            .endm

            .macro CLEAR_FLOAT_REGS, base
                fmv.d.x f0, x0
                fmv.d.x f1, x0
                fmv.d.x f2, x0
                fmv.d.x f3, x0
                fmv.d.x f4, x0
                fmv.d.x f5, x0
                fmv.d.x f5, x0
                fmv.d.x f6, x0
                fmv.d.x f7, x0
                fmv.d.x f8, x0
                fmv.d.x f9, x0
                fmv.d.x f10, x0
                fmv.d.x f11, x0
                fmv.d.x f12, x0
                fmv.d.x f13, x0
                fmv.d.x f14, x0
                fmv.d.x f15, x0
                fmv.d.x f16, x0
                fmv.d.x f17, x0
                fmv.d.x f18, x0
                fmv.d.x f19, x0
                fmv.d.x f20, x0
                fmv.d.x f21, x0
                fmv.d.x f22, x0
                fmv.d.x f23, x0
                fmv.d.x f24, x0
                fmv.d.x f25, x0
                fmv.d.x f26, x0
                fmv.d.x f27, x0
                fmv.d.x f28, x0
                fmv.d.x f29, x0
                fmv.d.x f30, x0
                fmv.d.x f31, x0
            .endm

            .endif"#
        )
    };
}

macro_rules! include_asm_macros {
    () => {
        concat!(
            __asm_macros!(),
            r#"
            .ifndef REGS_MACROS_FLAG
            .equ REGS_MACROS_FLAG, 1

            .macro PUSH_POP_GENERAL_REGS, op
                \op ra, sp, 1
                // gp (x3, slot-3) is intentionally NOT saved/restored here.
                // On RISC-V `gp` holds this hart's per-CPU base (percpu
                // crate); it is per-hart-invariant and set once at boot, so it
                // must never be stored in a per-task trapframe that can
                // migrate between harts between PUSH and POP -- doing so
                // restored a stale foreign base into `gp` and corrupted
                // `current()`. slot-3 is instead used to carry the *user* gp
                // across a U-mode trap: written by `.Lexit_user` on entry and
                // loaded by `.Ltrap_return` on U-mode return only. An S-mode
                // trap return leaves `gp` untouched.
                \op tp, sp, 4
                \op t0, sp, 5
                \op t1, sp, 6
                \op t2, sp, 7
                \op s0, sp, 8
                \op s1, sp, 9
                \op a0, sp, 10
                \op a1, sp, 11
                \op a2, sp, 12
                \op a3, sp, 13
                \op a4, sp, 14
                \op a5, sp, 15
                \op a6, sp, 16
                \op a7, sp, 17
                \op s2, sp, 18
                \op s3, sp, 19
                \op s4, sp, 20
                \op s5, sp, 21
                \op s6, sp, 22
                \op s7, sp, 23
                \op s8, sp, 24
                \op s9, sp, 25
                \op s10, sp, 26
                \op s11, sp, 27
                \op t3, sp, 28
                \op t4, sp, 29
                \op t5, sp, 30
                \op t6, sp, 31
            .endm

            .macro PUSH_GENERAL_REGS
                PUSH_POP_GENERAL_REGS STR
            .endm
            .macro POP_GENERAL_REGS
                PUSH_POP_GENERAL_REGS LDR
            .endm

            .endif"#
        )
    };
}
