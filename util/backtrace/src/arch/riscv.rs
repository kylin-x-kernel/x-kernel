// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V architecture support.

use core::arch::asm;

use super::ArchBacktrace;

/// RISC-V architecture implementation.
pub struct RiscV;

impl ArchBacktrace for RiscV {
    const FP_ALIGNMENT: usize = 8;
    const FRAME_OFFSET: usize = 1;

    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("addi {}, s0, 0", out(reg) fp, options(nomem, nostack)) };
        fp
    } // RISC-V uses 8-byte alignment
}
