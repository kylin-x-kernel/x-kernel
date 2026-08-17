// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>

// AArch64 guest trap vector table.
//
// Installed into VBAR_EL2 before entering guest.
// Each entry saves guest x0/x1 to the host stack, captures ESR_EL2 into
// vcpu->esr (offset 528), writes exit_type into vcpu->exit_type (offset 520
// via tpidr_el2), then branches to el2_trap_exit_common.
//
// ESR_EL2 is captured here — before the host unmasks IRQs in Rust — so a
// later host exception cannot clobber the syndrome for a synchronous exit.
//
// Exit types: 0=synchronous, 1=IRQ, 2=FIQ, 3=SError
//
// AArch64 vector table: 16 entries × 128 bytes each, 2KB aligned.
//   0x000: Current EL, SP_EL0
//   0x200: Current EL, SP_ELx
//   0x400: Lower EL, AArch64  ← guest EL1→EL2 traps
//   0x600: Lower EL, AArch32

.arch armv8.2-a

.macro KVMM_GUEST_ENTRY type
    .balign 128
    stp  x0, x1, [sp, #-16]!
    mrs  x0, tpidr_el2
    mrs  x1, esr_el2
    str  x1, [x0, #528]         // vcpu->esr
    mov  x1, #\type
    str  x1, [x0, #520]         // vcpu->exit_type
    b    el2_trap_exit_common
.endm

.section .text.kvmm_guest_vec
.balign 2048
.global kvmm_guest_vec_table
kvmm_guest_vec_table:
    // 0x000: Current EL + SP_EL0 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY 0
    KVMM_GUEST_ENTRY 1
    KVMM_GUEST_ENTRY 2
    KVMM_GUEST_ENTRY 3

    // 0x200: Current EL + SP_ELx — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY 0
    KVMM_GUEST_ENTRY 1
    KVMM_GUEST_ENTRY 2
    KVMM_GUEST_ENTRY 3

    // 0x400: Lower EL AArch64 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY 0
    KVMM_GUEST_ENTRY 1
    KVMM_GUEST_ENTRY 2
    KVMM_GUEST_ENTRY 3

    // 0x600: Lower EL AArch32 — Sync / IRQ / FIQ / SError
    KVMM_GUEST_ENTRY 0
    KVMM_GUEST_ENTRY 1
    KVMM_GUEST_ENTRY 2
    KVMM_GUEST_ENTRY 3
