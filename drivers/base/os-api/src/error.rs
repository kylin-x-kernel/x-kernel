// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Error returned by resource acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResError {
    /// The resource descriptor was invalid (e.g. zero-sized region).
    InvalidResource,
    /// The host failed to establish the requested mapping.
    MappingFailed,
    /// The host operation failed with an I/O error.
    Io,
    /// The host ran out of memory while acquiring the resource.
    NoMemory,
    /// The resource is already in use and cannot be shared.
    Busy,
    /// The host does not support this kind of resource.
    Unsupported,
    /// The capability is not in a state that permits this operation.
    BadState,
    /// A named capability is already registered.
    AlreadyExists,
    /// The requested capability is not present.
    NotFound,
}

/// Result of an OS capability operation.
pub type ResResult<T = ()> = Result<T, ResError>;
