// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// Atomically compare-exchange a 32-bit word at a (possibly user) address.
//
// Prototype:
//   size_t user_atomic_cmpxchg_u32(u32 *addr, u32 old, u32 new, u32 *old_out);
// Args: x0=addr, w1=old, w2=new, x3=old_out
// Returns: 0 if no fault occurred; non-zero if a data abort occurred.
// On success, *old_out receives the value observed at *addr. If that value
// equals `old`, `new` was stored.

.section .text
.global user_atomic_cmpxchg_u32
.type user_atomic_cmpxchg_u32, @function

user_atomic_cmpxchg_u32:
.Lcmpxchg_retry:
1:  ldxr    w4, [x0]
    // old_out is a kernel pointer; no exception table needed.
    str     w4, [x3]
    cmp     w4, w1
    b.ne    .Lcmpxchg_done
2:  stxr    w5, w2, [x0]
    cbnz    w5, .Lcmpxchg_retry
    dmb     ish
.Lcmpxchg_done:
    clrex
    mov     x0, #0
    ret

.Lcmpxchg_fault:
    clrex
    mov     x0, #1
    ret

    _asm_extable 1b, .Lcmpxchg_fault
    _asm_extable 2b, .Lcmpxchg_fault

.size user_atomic_cmpxchg_u32, .-user_atomic_cmpxchg_u32
