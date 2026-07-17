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

.section .text
.global user_atomic_load_u32
.type user_atomic_load_u32, @function

// Atomically load a naturally aligned user u32.
// Args: a0=addr, a1=value_out
// Returns: 0 if no fault occurred; non-zero if a fault occurred.
user_atomic_load_u32:
1:  lw      t0, (a0)
    sw      t0, 0(a1)
    li      a0, 0
    ret

.Latomic_load_fault:
    li      a0, 1
    ret

    _asm_extable 1b, .Latomic_load_fault

.size user_atomic_load_u32, .-user_atomic_load_u32

.global user_atomic_cmpxchg_u32
.type user_atomic_cmpxchg_u32, @function

user_atomic_cmpxchg_u32:
.Lcmpxchg_retry:
1:  lr.w    t0, (a0)
    sw      t0, 0(a3)
    bne     t0, a1, .Lcmpxchg_done
2:  sc.w    t1, a2, (a0)
    bnez    t1, .Lcmpxchg_retry
.Lcmpxchg_done:
    fence   rw, rw
    li      a0, 0
    ret

.Lcmpxchg_fault:
    li      a0, 1
    ret

    _asm_extable 1b, .Lcmpxchg_fault
    _asm_extable 2b, .Lcmpxchg_fault

.size user_atomic_cmpxchg_u32, .-user_atomic_cmpxchg_u32
