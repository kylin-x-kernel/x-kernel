// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stack frame representation.

use core::fmt;

use crate::{
    arch::{ArchBacktrace, CurrentArch},
    error::{BacktraceError, InvalidReason, Result},
};

/// Represents a single stack frame in the unwound stack.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame {
    /// The frame pointer of the previous stack frame.
    pub fp: usize,

    /// The instruction pointer (return address).
    pub ip: usize,
}

impl Frame {
    /// Read a frame from the given frame pointer.
    ///
    /// # Safety
    ///
    /// This function is safe if the frame pointer is valid and points to
    /// a properly formatted stack frame.
    pub fn read(fp: usize) -> Result<Self> {
        // Validate frame pointer
        CurrentArch::validate_fp(fp)?;

        // Read frame from memory
        let frame = unsafe { Self::read_unchecked(fp) };

        // Validate the read frame
        if !frame.is_valid() {
            return Err(BacktraceError::InvalidFramePointer {
                fp: frame.fp,
                reason: InvalidReason::InvalidMemory,
            });
        }

        Ok(frame)
    }

    /// Read a frame without validation (unsafe).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `fp` is valid and properly aligned.
    unsafe fn read_unchecked(fp: usize) -> Self {
        let offset = CurrentArch::FRAME_OFFSET;
        unsafe { (fp as *const Frame).sub(offset).read() }
    }

    /// Adjust the instruction pointer for symbolication.
    ///
    /// The IP in a frame points to the return address (the instruction after
    /// the call). To symbolicate correctly, we need to subtract 1 to get the
    /// address of the call instruction.
    ///
    /// See: https://github.com/rust-lang/backtrace-rs/blob/master/src/symbolize/mod.rs#L145
    pub const fn adjust_ip(&self) -> usize {
        self.ip.saturating_sub(1)
    }

    /// Check if this frame appears valid.
    pub const fn is_valid(&self) -> bool {
        self.fp != 0 && self.ip != 0
    }

    /// Create a new frame.
    pub const fn new(fp: usize, ip: usize) -> Self {
        Self { fp, ip }
    }
}

impl fmt::Display for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fp={:#018x}, ip={:#018x}", self.fp, self.ip)
    }
}
