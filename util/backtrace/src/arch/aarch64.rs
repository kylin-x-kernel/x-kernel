// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 (ARM64) architecture support.

use core::arch::asm;

use super::ArchBacktrace;

/// AArch64 architecture implementation.
pub struct AArch64;

impl ArchBacktrace for AArch64 {
    const FP_ALIGNMENT: usize = 16;
    const FRAME_OFFSET: usize = 0;

    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
        fp
    } // AArch64 requires 16-byte stack alignment
}
