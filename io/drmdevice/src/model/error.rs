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
    /// Allocation failed.
    NoMemory,
    /// The backend rejected a hardware operation.
    Io,
    /// A user pointer could not be accessed.
    BadAddress,
    /// Non-blocking operation has no data yet.
    WouldBlock,
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
            Self::NoMemory => VfsError::NoMemory,
            Self::Io => VfsError::Io,
            Self::BadAddress => VfsError::BadAddress,
            Self::WouldBlock => VfsError::WouldBlock,
        }
    }
}

impl From<DrmError> for VfsError {
    fn from(error: DrmError) -> Self {
        error.to_vfs()
    }
}
