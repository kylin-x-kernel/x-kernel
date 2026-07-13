// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! # Error handling module
//!
//! Defines all error types used in the rsext4 library, providing clear error
//! messages for debugging and handling.

/// Block device error type
///
/// All possible block device operation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockDevError {
    /// Invalid input
    InvalidInput,
    /// Target path already exists
    AlreadyExists,
    /// Target path does not exist (ENOENT)
    NotFound,
    /// Operation not permitted (EPERM)
    NotPermitted,
    /// Expected a directory but the target is not a directory
    NotDirectory,
    /// Expected a non-directory but the target is a directory
    IsDirectory,
    /// Directory not empty
    DirectoryNotEmpty,
    /// Read error
    ReadError,
    /// Write error
    WriteError,
    /// Block number out of range
    BlockOutOfRange { block_id: u32, max_blocks: u64 },
    /// Invalid block size
    InvalidBlockSize { size: usize, expected: usize },
    /// Buffer too small
    BufferTooSmall { provided: usize, required: usize },
    /// Device not open
    DeviceNotOpen,
    /// Device already closed
    DeviceClosed,
    /// I/O error
    IoError,
    /// Alignment error (data not aligned to block boundary)
    AlignmentError { offset: u64, alignment: u32 },
    /// Device busy
    DeviceBusy,
    /// Timeout
    Timeout,
    /// Unsupported operation
    Unsupported,
    /// Device is read-only
    ReadOnly,
    /// No space left
    NoSpace,
    /// Permission denied
    PermissionDenied,
    /// Device or data corrupted
    Corrupted,
    /// Checksum error
    ChecksumError,
    /// Unknown error
    Unknown,
}

impl core::fmt::Display for BlockDevError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BlockDevError::InvalidInput => {
                write!(f, "invalid input")
            }
            BlockDevError::AlreadyExists => write!(f, "target already exists"),
            BlockDevError::NotFound => write!(f, "no such file or directory"),
            BlockDevError::NotPermitted => write!(f, "operation not permitted"),
            BlockDevError::NotDirectory => write!(f, "not a directory"),
            BlockDevError::IsDirectory => write!(f, "is a directory"),
            BlockDevError::DirectoryNotEmpty => write!(f, "directory not empty"),
            BlockDevError::ReadError => write!(f, "failed to read from block device"),
            BlockDevError::WriteError => write!(f, "failed to write to block device"),
            BlockDevError::BlockOutOfRange {
                block_id,
                max_blocks,
            } => {
                write!(f, "block id {block_id} out of range (max {max_blocks})")
            }
            BlockDevError::InvalidBlockSize { size, expected } => {
                write!(f, "invalid block size {size} (expected {expected})")
            }
            BlockDevError::BufferTooSmall { provided, required } => {
                write!(
                    f,
                    "buffer too small: provided {provided} bytes, required {required} bytes"
                )
            }
            BlockDevError::DeviceNotOpen => write!(f, "device not open"),
            BlockDevError::DeviceClosed => write!(f, "device already closed"),
            BlockDevError::IoError => write!(f, "I/O error"),
            BlockDevError::AlignmentError { offset, alignment } => {
                write!(
                    f,
                    "alignment error: offset {offset} is not aligned to {alignment}-byte boundary"
                )
            }
            BlockDevError::DeviceBusy => write!(f, "device is busy"),
            BlockDevError::Timeout => write!(f, "operation timed out"),
            BlockDevError::Unsupported => write!(f, "unsupported operation"),
            BlockDevError::ReadOnly => write!(f, "device is read-only"),
            BlockDevError::NoSpace => write!(f, "no space left on device"),
            BlockDevError::PermissionDenied => write!(f, "permission denied"),
            BlockDevError::Corrupted => write!(f, "device or data is corrupted"),
            BlockDevError::ChecksumError => write!(f, "checksum error"),
            BlockDevError::Unknown => write!(f, "unknown error"),
        }
    }
}
/// Block device operation result type
pub type BlockDevResult<T> = Result<T, BlockDevError>;

/// Ext4 filesystem error
///
/// All possible ext4 filesystem operation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RSEXT4Error {
    /// I/O error
    IoError,
    /// Invalid magic number
    InvalidMagic,
    /// Invalid superblock (e.g. GDT beyond reserved space)
    InvalidSuperblock,
    /// Filesystem has errors
    FilesystemHasErrors,
    /// Unsupported feature bits (incompat or ro-compat)
    UnsupportedFeature { bits: u32 },
    /// Already mounted
    AlreadyMounted,
}

impl core::fmt::Display for RSEXT4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RSEXT4Error::IoError => write!(f, "I/O error"),
            RSEXT4Error::InvalidMagic => write!(f, "invalid magic number"),
            RSEXT4Error::InvalidSuperblock => write!(f, "invalid superblock"),
            RSEXT4Error::FilesystemHasErrors => write!(f, "filesystem has errors"),
            RSEXT4Error::UnsupportedFeature { bits } => {
                write!(f, "unsupported feature: {bits:#x}")
            }
            RSEXT4Error::AlreadyMounted => write!(f, "filesystem already mounted"),
        }
    }
}

/// Ext4 filesystem operation result type
pub type Ext4Result<T> = Result<T, RSEXT4Error>;
