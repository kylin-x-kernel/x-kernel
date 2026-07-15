// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// Atomically compare-exchange a 32-bit word at a (possibly user) address.
//
// Prototype:
//   size_t user_atomic_cmpxchg_u32(u32 *addr, u32 old, u32 new, u32 *old_out);
// Args: a0=addr, a1=old, a2=new, a3=old_out
// Returns: 0 if no fault occurred; non-zero if a fault occurred.
// On success, *old_out receives the value observed at *addr. If that value
// equals `old`, `new` was stored.
//
// LoongArch `sc.w rd, rj, si12` overwrites `rd` with 1 on success / 0 on fail,
// so the desired store value must be reloaded into `rd` on each retry.

.section .text
.global user_atomic_cmpxchg_u32
.type user_atomic_cmpxchg_u32, @function

user_atomic_cmpxchg_u32:
.Lcmpxchg_retry:
1:  ll.w    $t0, $a0, 0
    st.w    $t0, $a3, 0
    bne     $t0, $a1, .Lcmpxchg_done
    move    $t1, $a2
2:  sc.w    $t1, $a0, 0
    beqz    $t1, .Lcmpxchg_retry
    dbar    0
.Lcmpxchg_done:
    move    $a0, $zero
    jr      $ra

.Lcmpxchg_fault:
    ori     $a0, $zero, 1
    jr      $ra

    _asm_extable 1b, .Lcmpxchg_fault
    _asm_extable 2b, .Lcmpxchg_fault

.size user_atomic_cmpxchg_u32, .-user_atomic_cmpxchg_u32
