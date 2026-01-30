// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 architecture support.

use core::arch::asm;

use super::ArchBacktrace;

/// LoongArch64 architecture implementation.
pub struct LoongArch64;

impl ArchBacktrace for LoongArch64 {
    const FP_ALIGNMENT: usize = 8;
    const FRAME_OFFSET: usize = 1;

    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("move {}, $fp", out(reg) fp, options(nomem, nostack)) };
        fp
    }
}
