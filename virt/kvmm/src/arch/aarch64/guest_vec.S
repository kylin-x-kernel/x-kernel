// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 guest trap vector table.
//
// Installed into VBAR_EL2 (via vbar_el1 VHE alias) before entering guest.
// All 16 entries branch to el2_trap_exit.
//
// AArch64 vector table: 16 entries × 128 bytes each, 2KB aligned.
//   0x000: Current EL, SP_EL0
//   0x200: Current EL, SP_ELx
//   0x400: Lower EL, AArch64  ← guest EL1→EL2 traps
//   0x600: Lower EL, AArch32

.arch armv8.2-a

.macro KVMM_GUEST_ENTRY
    .balign 128
    b  el2_trap_exit
.endm

.section .text.kvmm_guest_vec
.balign 2048
.global kvmm_guest_vec_table
kvmm_guest_vec_table:
    // 0x000: Current EL + SP_EL0 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY

    // 0x200: Current EL + SP_ELx — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY

    // 0x400: Lower EL AArch64 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY

    // 0x600: Lower EL AArch32 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
    KVMM_GUEST_ENTRY
