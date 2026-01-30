// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-specific backtrace support.

use crate::error::{BacktraceError, InvalidReason, Result};

/// Architecture-specific backtrace operations.
pub trait ArchBacktrace {
    /// Get the current frame pointer.
    fn current_fp() -> usize;

    /// Frame offset for reading frames on this architecture.
    const FRAME_OFFSET: usize;

    /// Required alignment for frame pointers.
    const FP_ALIGNMENT: usize;

    /// Validate a frame pointer for this architecture.
    fn validate_fp(fp: usize) -> Result<()> {
        if fp == 0 {
            return Err(BacktraceError::InvalidFramePointer {
                fp,
                reason: InvalidReason::Null,
            });
        }

        if !fp.is_multiple_of(Self::FP_ALIGNMENT) {
            return Err(BacktraceError::InvalidFramePointer {
                fp,
                reason: InvalidReason::Misaligned,
            });
        }

        Ok(())
    }
}

// Architecture-specific implementations
#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "loongarch64")]
mod loongarch64;
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
mod riscv;
#[cfg(target_arch = "x86_64")]
mod x86_64;

// Re-export current architecture
#[cfg(target_arch = "aarch64")]
pub use aarch64::AArch64 as CurrentArch;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::LoongArch64 as CurrentArch;
#[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
pub use riscv::RiscV as CurrentArch;
#[cfg(target_arch = "x86_64")]
pub use x86_64::X86_64 as CurrentArch;
