//! x86_64 architecture support.

use super::ArchBacktrace;
use core::arch::asm;

/// x86_64 architecture implementation.
pub struct X86_64;

impl ArchBacktrace for X86_64 {
    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack)) };
        fp
    }

    const FRAME_OFFSET: usize = 0;
    const FP_ALIGNMENT: usize = 16; // x86_64 requires 16-byte stack alignment
}
