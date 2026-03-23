// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 architecture support.

use core::arch::asm;

use super::ArchBacktrace;

/// x86_64 architecture implementation.
pub struct X86_64;

impl ArchBacktrace for X86_64 {
    // x86_64 only requires frame pointers to be naturally pointer-aligned.
    // Requiring 16-byte alignment rejects valid `rbp` chains in optimized code.
    const FP_ALIGNMENT: usize = core::mem::size_of::<usize>();
    const FRAME_OFFSET: usize = 0;

    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack)) };
        fp
    } // x86_64 requires 16-byte stack alignment
}
