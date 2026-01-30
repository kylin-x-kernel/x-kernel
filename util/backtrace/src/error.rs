// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for backtrace operations.

use core::fmt;

/// Errors that can occur during backtrace operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacktraceError {
    /// Backtrace library not initialized.
    NotInitialized,

    /// Invalid frame pointer.
    InvalidFramePointer { fp: usize, reason: InvalidReason },

    /// Frame pointer out of valid range.
    OutOfRange { fp: usize, range: (usize, usize) },

    /// Stack appears too large (potential infinite loop).
    StackTooLarge {
        fp: usize,
        prev_fp: usize,
        size: usize,
    },

    /// Architecture not supported.
    UnsupportedArchitecture,

    /// DWARF symbolication not available.
    DwarfUnavailable,
}

/// Reasons why a frame pointer might be invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    /// Frame pointer is null.
    Null,
    /// Frame pointer is not properly aligned.
    Misaligned,
    /// Frame pointer points to invalid memory.
    InvalidMemory,
    /// Frame pointer creates a cycle.
    Cycle,
}

impl fmt::Display for BacktraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInitialized => {
                write!(
                    f,
                    "Backtrace not initialized. Call backtrace::init() first."
                )
            }
            Self::InvalidFramePointer { fp, reason } => {
                write!(f, "Invalid frame pointer {:#x}: {:?}", fp, reason)
            }
            Self::OutOfRange { fp, range } => {
                write!(
                    f,
                    "Frame pointer {:#x} out of range [{:#x}, {:#x})",
                    fp, range.0, range.1
                )
            }
            Self::StackTooLarge { fp, prev_fp, size } => {
                write!(
                    f,
                    "Stack too large: {:#x} bytes between {:#x} and {:#x}",
                    size, prev_fp, fp
                )
            }
            Self::UnsupportedArchitecture => {
                write!(f, "Backtrace not supported on this architecture")
            }
            Self::DwarfUnavailable => {
                write!(f, "DWARF symbolication not available")
            }
        }
    }
}

/// Result type for backtrace operations.
pub type Result<T> = core::result::Result<T, BacktraceError>;
