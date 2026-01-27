//! AArch64 (ARM64) architecture support.

use super::ArchBacktrace;
use core::arch::asm;

/// AArch64 architecture implementation.
pub struct AArch64;

impl ArchBacktrace for AArch64 {
    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
        fp
    }

    const FRAME_OFFSET: usize = 0;
    const FP_ALIGNMENT: usize = 16; // AArch64 requires 16-byte stack alignment
}
