// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 architecture support.

use core::arch::asm;

use super::ArchBacktrace;

/// x86_64 architecture implementation.
pub struct X86_64;

impl ArchBacktrace for X86_64 {
    const FP_ALIGNMENT: usize = 16;
    const FRAME_OFFSET: usize = 0;

    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack)) };
        fp
    } // x86_64 requires 16-byte stack alignment
}
