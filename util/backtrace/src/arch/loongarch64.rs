//! LoongArch64 architecture support.

use super::ArchBacktrace;
use core::arch::asm;

/// LoongArch64 architecture implementation.
pub struct LoongArch64;

impl ArchBacktrace for LoongArch64 {
    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("move {}, $fp", out(reg) fp, options(nomem, nostack)) };
        fp
    }

    const FRAME_OFFSET: usize = 1;
    const FP_ALIGNMENT: usize = 8;
}
