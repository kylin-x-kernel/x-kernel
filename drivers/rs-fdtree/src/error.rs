// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for minimal FDT parsing.

/// Possible errors when working with a Flattened Device Tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FdtError {
    BadMagic,
    BadPtr,
    BufferTooSmall,
}

impl core::fmt::Display for FdtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FdtError::BadMagic => write!(f, "bad FDT magic value"),
            FdtError::BadPtr => write!(f, "an invalid pointer was passed"),
            FdtError::BufferTooSmall => write!(f, "the given buffer was too small"),
        }
    }
}

/// Convenience type alias for Result with FdtError
#[allow(dead_code)]
pub type Result<T> = core::result::Result<T, FdtError>;
