// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared AArch64 SMCCC call helpers.

core::arch::global_asm!(include_str!("smccc_trap.S"));

/// SMCCC success return value in `x0`.
pub const SMCCC_RET_SUCCESS: usize = 0;

/// SMCCC not-supported / conduit-unavailable return value in `x0`.
pub const SMCCC_RET_NOT_SUPPORTED: usize = usize::MAX;

/// Raw return registers captured from an SMCCC call.
///
/// Layout must stay `#[repr(C)]` with four `usize` fields in `x0..x3` order so
/// the assembly in `smccc_trap.S` can store return registers with `stp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmcccResult {
    pub x0: usize,
    pub x1: usize,
    pub x2: usize,
    pub x3: usize,
}

// Declared by `smccc_trap.S` (included above via `global_asm!`). Calling these
// is unsafe: they write through `out` and rely on the SMCCC / exception-table
// contract documented at each call site.
unsafe extern "C" {
    fn aarch64_peripherals_smccc_hvc_call(
        func: u32,
        arg0: usize,
        arg1: usize,
        arg2: usize,
        out: *mut SmcccResult,
    );
    fn aarch64_peripherals_smccc_smc_call(
        func: u32,
        arg0: usize,
        arg1: usize,
        arg2: usize,
        out: *mut SmcccResult,
    );
}

/// Invoke an SMCCC HVC call and return the raw `x0`..`x3` results.
pub fn hvc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let mut out = SmcccResult::default();
    // SAFETY:
    // 1. `out` is a stack-local `SmcccResult`; `&mut out` is aligned, exclusive,
    //    and valid for the duration of the call.
    // 2. `SmcccResult` is `#[repr(C)]` with four `usize` fields (`x0..x3`),
    //    matching the assembly which stores return registers via
    //    `stp x0,x1,[out]` and `stp x2,x3,[out,#16]`.
    // 3. Arguments follow the SMCCC calling convention (`x0`=func,
    //    `x1..x3`=args); the helper moves `out` into `x8` before the HVC.
    // 4. `smccc_trap.S` registers an `__ex_table` entry for the `hvc` site so
    //    an unavailable conduit traps into a fixup that writes
    //    `SMCCC_RET_NOT_SUPPORTED` into `out.x0` instead of panicking.
    unsafe {
        aarch64_peripherals_smccc_hvc_call(func, arg0, arg1, arg2, &mut out);
    }
    out
}

/// Invoke an SMCCC SMC call and return the raw `x0`..`x3` results.
pub fn smc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> SmcccResult {
    let mut out = SmcccResult::default();
    // SAFETY:
    // 1. `out` is a stack-local `SmcccResult`; `&mut out` is aligned, exclusive,
    //    and valid for the duration of the call.
    // 2. `SmcccResult` is `#[repr(C)]` with four `usize` fields (`x0..x3`),
    //    matching the assembly which stores return registers via
    //    `stp x0,x1,[out]` and `stp x2,x3,[out,#16]`.
    // 3. Arguments follow the SMCCC calling convention (`x0`=func,
    //    `x1..x3`=args); the helper moves `out` into `x8` before the SMC.
    // 4. `smccc_trap.S` registers an `__ex_table` entry for the `smc` site so
    //    an unavailable conduit traps into a fixup that writes
    //    `SMCCC_RET_NOT_SUPPORTED` into `out.x0` instead of panicking.
    unsafe {
        aarch64_peripherals_smccc_smc_call(func, arg0, arg1, arg2, &mut out);
    }
    out
}
