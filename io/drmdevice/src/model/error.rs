// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM error domain.
//!
//! Command handlers and the object model report [`DrmError`]; the `DeviceFileOps`
//! adapter in `device/minor.rs` maps it to the VFS error the syscall layer expects.
//! Keeping the two apart is what lets the ioctl matrix be unit-testable without
//! a filesystem.

use kvfs::VfsError;

/// Failure of one DRM operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrmError {
    /// Malformed request: bad id, geometry, format or property value.
    InvalidInput,
    /// The command exists but this node/driver does not implement it.
    OpNotSupported,
    /// The bound display is gone.
    NoSuchDevice,
    /// Another client owns the resource.
    Busy,
    /// The caller is not the master (or the authenticated client) it must be.
    PermissionDenied,
    /// The caller does not own the object it names.
    NotPermitted,
    /// The referenced object does not exist.
    NotFound,
    /// The object already exists and cannot be created twice.
    AlreadyExists,
    /// Allocation failed.
    NoMemory,
    /// The backend rejected a hardware operation.
    Io,
    /// A user pointer could not be accessed.
    BadAddress,
    /// Non-blocking operation has no data yet.
    WouldBlock,
    /// A blocking request was interrupted by a signal.
    Interrupted,
    /// A wait did not finish before its deadline.
    TimedOut,
}

pub(crate) type DrmResult<T> = Result<T, DrmError>;

impl DrmError {
    /// Maps the DRM error onto the errno the syscall layer reports.
    pub(crate) const fn to_vfs(self) -> VfsError {
        match self {
            Self::InvalidInput => VfsError::InvalidInput,
            Self::OpNotSupported => VfsError::OperationNotSupported,
            Self::NoSuchDevice => VfsError::NoSuchDevice,
            Self::Busy => VfsError::ResourceBusy,
            Self::PermissionDenied => VfsError::PermissionDenied,
            Self::NotPermitted => VfsError::OperationNotPermitted,
            Self::NotFound => VfsError::NotFound,
            Self::AlreadyExists => VfsError::AlreadyExists,
            Self::NoMemory => VfsError::NoMemory,
            Self::Io => VfsError::Io,
            Self::BadAddress => VfsError::BadAddress,
            Self::WouldBlock => VfsError::WouldBlock,
            Self::Interrupted => VfsError::Interrupted,
            Self::TimedOut => VfsError::TimedOut,
        }
    }
}

impl DrmError {
    /// The errno a fence records for this error.
    pub(crate) const fn errno(self) -> i32 {
        match self {
            Self::InvalidInput => 22,
            Self::OpNotSupported => 95,
            Self::NoSuchDevice => 19,
            Self::Busy => 16,
            Self::PermissionDenied => 13,
            Self::NotPermitted => 1,
            Self::NotFound => 2,
            Self::AlreadyExists => 17,
            Self::NoMemory => 12,
            Self::Io => 5,
            Self::BadAddress => 14,
            Self::WouldBlock => 11,
            Self::Interrupted => 4,
            Self::TimedOut => 110,
        }
    }

    /// The error a fence reports as its failure.
    ///
    /// A fence error is an errno chosen by the party that failed the work; it
    /// is reported unchanged when it names a known errno, because the caller of
    /// `SYNCOBJ_WAIT` or a sync_file read sees the producer's failure and not
    /// this driver's. An unknown value is a plain I/O failure.
    pub(crate) const fn from_fence(error: i32) -> Self {
        match error {
            4 => Self::Interrupted,
            11 => Self::WouldBlock,
            12 => Self::NoMemory,
            16 => Self::Busy,
            17 => Self::AlreadyExists,
            19 => Self::NoSuchDevice,
            22 => Self::InvalidInput,
            110 => Self::TimedOut,
            // Every other errno, including a cancellation no fence of this
            // driver produces, is reported as a plain I/O failure.
            _ => Self::Io,
        }
    }
}

impl From<DrmError> for VfsError {
    fn from(error: DrmError) -> Self {
        error.to_vfs()
    }
}
