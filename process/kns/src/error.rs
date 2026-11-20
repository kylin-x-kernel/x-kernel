// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for namespace operations.

/// Errors that can occur when cloning a child [`crate::NsProxy`].
///
/// Carries enough information for the syscall layer to translate each failure
/// into the correct errno instead of collapsing them into a single value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloneNsError {
    /// Mount namespace cloning was requested with a shared filesystem context
    /// (`CLONE_NEWNS` with `CLONE_FS`).
    InvalidFlagCombination,
    /// Creating the requested namespace requires `CAP_SYS_ADMIN` in the
    /// caller's user namespace.
    NotPermitted,
    /// Mount namespace copy or filesystem-context retargeting failed.
    Mount(kvfs::VfsError),
    /// A resource cap was reached (user-namespace nesting depth, ENOSPC).
    NoSpace,
}

/// Errors that can occur when setting a UTS namespace name (nodename or
/// domainname).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UtsError {
    /// The supplied name exceeds the maximum length (64 bytes; the 65-byte
    /// buffer reserves one slot for the NUL terminator).
    NameTooLong,
}
