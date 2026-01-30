// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for FDT parsing and manipulation.
//!
//! This module defines the error types that can occur when working with
//! Flattened Device Trees. All errors implement `Display` for user-friendly
//! error messages.

/// Possible errors when working with a Flattened Device Tree
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FdtError {
    /// The FDT had an invalid magic value
    BadMagic,
    /// The given pointer was null
    BadPtr,
    /// The slice passed in was too small to fit the given total size of the FDT
    /// structure
    BufferTooSmall,
    /// Invalid UTF-8 string encountered
    InvalidString,
    /// Invalid or missing property
    InvalidProperty,
    /// Failed to parse a value
    ParseError,
    /// Invalid cell size configuration
    InvalidCellSize,
    /// Required node not found
    NodeNotFound,
    /// Invalid C string (no null terminator)
    InvalidCString,
    /// Unexpected token or structure
    UnexpectedToken,
    /// Buffer underflow or overflow
    BufferError,
}

impl core::fmt::Display for FdtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FdtError::BadMagic => write!(f, "bad FDT magic value"),
            FdtError::BadPtr => write!(f, "an invalid pointer was passed"),
            FdtError::BufferTooSmall => {
                write!(f, "the given buffer was too small to contain a FDT header")
            }
            FdtError::InvalidString => write!(f, "invalid UTF-8 string"),
            FdtError::InvalidProperty => write!(f, "invalid or missing property"),
            FdtError::ParseError => write!(f, "failed to parse value"),
            FdtError::InvalidCellSize => write!(f, "invalid cell size configuration"),
            FdtError::NodeNotFound => write!(f, "required node not found"),
            FdtError::InvalidCString => write!(f, "invalid C string (no null terminator)"),
            FdtError::UnexpectedToken => write!(f, "unexpected token or structure"),
            FdtError::BufferError => write!(f, "buffer underflow or overflow"),
        }
    }
}

/// Convenience type alias for Result with FdtError
#[allow(dead_code)]
pub type Result<T> = core::result::Result<T, FdtError>;
