// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
//
// Atomically compare-exchange a 32-bit word at a (possibly user) address.
//
// Prototype:
//   size_t user_atomic_cmpxchg_u32(u32 *addr, u32 old, u32 new, u32 *old_out);
// Args: rdi=addr, esi=old, edx=new, rcx=old_out
// Returns: 0 if no fault occurred; non-zero if a fault occurred.
// On success, *old_out receives the value observed at *addr. If that value
// equals `old`, `new` was stored.

.section .text
.global user_atomic_cmpxchg_u32
.type user_atomic_cmpxchg_u32, @function

user_atomic_cmpxchg_u32:
    mov     eax, esi
1:  lock cmpxchg dword ptr [rdi], edx
    // After cmpxchg, eax always holds the previous *addr value.
    mov     dword ptr [rcx], eax
    xor     eax, eax
    ret

.Lcmpxchg_fault:
    mov     eax, 1
    ret

    _asm_extable 1b, .Lcmpxchg_fault

.size user_atomic_cmpxchg_u32, .-user_atomic_cmpxchg_u32
