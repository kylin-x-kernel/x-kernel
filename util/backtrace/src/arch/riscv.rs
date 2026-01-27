//! RISC-V architecture support.

use super::ArchBacktrace;
use core::arch::asm;

/// RISC-V architecture implementation.
pub struct RiscV;

impl ArchBacktrace for RiscV {
    fn current_fp() -> usize {
        let fp: usize;
        unsafe { asm!("addi {}, s0, 0", out(reg) fp, options(nomem, nostack)) };
        fp
    }

    const FRAME_OFFSET: usize = 1;
    const FP_ALIGNMENT: usize = 8; // RISC-V uses 8-byte alignment
}
