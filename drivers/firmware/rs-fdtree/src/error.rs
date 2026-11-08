// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for minimal FDT parsing.

/// Possible errors when working with a Flattened Device Tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FdtError {
    /// The blob's header magic is not `0xd00dfeed`.
    BadMagic,
    /// A null pointer was passed where a DTB pointer was required.
    BadPtr,
    /// The buffer is shorter than the header-declared total size.
    BufferTooSmall,
    /// Invalid DTB layout or structure, or a format/depth this parser cannot traverse.
    ParseError,
}

impl core::fmt::Display for FdtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FdtError::BadMagic => write!(f, "bad FDT magic value"),
            FdtError::BadPtr => write!(f, "an invalid pointer was passed"),
            FdtError::BufferTooSmall => write!(f, "the given buffer was too small"),
            FdtError::ParseError => write!(f, "invalid or unsupported FDT structure"),
        }
    }
}
