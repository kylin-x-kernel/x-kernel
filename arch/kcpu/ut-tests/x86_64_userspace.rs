// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

/// Writes a 64-bit value into XMM0.
#[cfg(feature = "fp-simd")]
fn write_xmm0(value: u64) {
    // SAFETY: `movq` moves a general register into XMM0; the `out("xmm0")`
    // operand tells the compiler the register is clobbered.
    unsafe {
        core::arch::asm!("movq xmm0, {v}", v = in(reg) value, out("xmm0") _);
    }
}

/// Reads the low 64 bits of XMM0.
#[cfg(feature = "fp-simd")]
fn read_xmm0() -> u64 {
    let value: u64;
    // SAFETY: reads the current XMM0; declaring it `out` is conservative
    // but lets the compiler reference the register.
    unsafe {
        core::arch::asm!("movq {v}, xmm0", v = out(reg) value, out("xmm0") _);
    }
    value
}

#[cfg(feature = "fp-simd")]
#[def_test]
fn test_user_restorable_context_round_trips_fp_state() {
    let ctx = UserContext::new(0x1004, VirtAddr::from_usize(0x2000), 7);

    write_xmm0(0x1122_3344_5566_7788);
    let reset = MxCsr::default();
    x86_64::registers::mxcsr::write(reset);

    let saved = ctx.save_user_restorable();

    // Clobber the live FP/SIMD state the way a signal handler would.
    write_xmm0(0);
    x86_64::registers::mxcsr::write(reset & !MxCsr::PRECISION_MASK);

    let mut restored = ctx;
    restored.restore_user_restorable(saved);

    assert_eq!(
        read_xmm0(),
        0x1122_3344_5566_7788,
        "XMM0 must be restored from the signal restorable context"
    );
    assert_eq!(
        x86_64::registers::mxcsr::read(),
        reset,
        "MXCSR must be restored from the signal restorable context"
    );
}

#[cfg(feature = "fp-simd")]
#[def_test]
fn test_restore_masks_unsupported_mxcsr_bits() {
    let mut ctx = UserContext::new(0x1004, VirtAddr::from_usize(0x2000), 7);
    let mut saved = ctx.save_user_restorable();
    saved.fp.sse_mxcsr = u32::MAX;

    // Must not raise #GP in kernel mode.
    ctx.restore_user_restorable(saved);

    assert_eq!(
        x86_64::registers::mxcsr::read().bits() & !mxcsr_feature_mask(),
        0,
        "unsupported MXCSR bits must be cleared before FXRSTOR"
    );
}
