// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for namespace operations.

/// Errors that can occur when cloning a child [`crate::NsProxy`].
///
/// Carries enough information for the syscall layer to translate each failure
/// into the correct errno instead of collapsing them into a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneNsError {
    /// An invalid flag combination was requested (e.g. `CLONE_NEWNS |
    /// CLONE_FS`, or `CLONE_NEWPID | CLONE_PARENT`).
    InvalidFlagCombination,
    /// A namespace flag was requested that has no implementation yet (e.g.
    /// `CLONE_NEWNET`, `CLONE_NEWUSER`, `CLONE_NEWCGROUP`, `CLONE_NEWPID`,
    /// `CLONE_NEWTIME`).
    Unimplemented,
    /// Mount namespace copy or filesystem-context retargeting failed.
    Mount(kvfs::VfsError),
}

/// Errors that can occur when setting a UTS namespace name (nodename or
/// domainname).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtsError {
    /// The supplied name exceeds the maximum length (64 bytes; the 65-byte
    /// buffer reserves one slot for the NUL terminator).
    NameTooLong,
}
