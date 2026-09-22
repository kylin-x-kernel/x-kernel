// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stub base access for host-side tests on unsupported targets.
//!
//! The architecture selector only falls back here under `cfg(test)`, so real
//! non-test builds on unsupported architectures still fail with
//! `compile_error!`. Tests reach slot storage through pins that override
//! `base`, and dynamic slots only use the pin's CPU identity, so the stub
//! value is never dereferenced.

#[inline]
pub(crate) fn current_base() -> usize {
    0
}

// Scalar single-instruction operations. These architectures have no
// per-CPU-base offset addressing instruction, so the operations mask local
// IRQs through `karch`'s `CpuLocalIrqIf` provider around a plain access.
crate::single_instr::impl_single_instr_fallback_arith!(
    u8, u16, u32, u64, usize, i8, i16, i32, i64, isize
);
crate::single_instr::impl_single_instr_fallback_load_store!(
    u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, bool
);
