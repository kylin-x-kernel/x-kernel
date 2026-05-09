// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for POSIX credential syscalls.

use kcred::{CredentialError, Credentials};
use kerrno::KError;
use ktask::current;
use kthread::AsThread;

/// Linux syscall value meaning "do not change this ID".
pub(crate) const NO_CHANGE_ID: u32 = u32::MAX;

/// Converts a Linux set-ID argument into an optional credential ID.
pub(crate) fn optional_id(id: u32) -> Option<u32> {
    (id != NO_CHANGE_ID).then_some(id)
}

/// Runs a closure with a read-only snapshot of the current process credentials.
pub(crate) fn with_credentials<R>(f: impl FnOnce(&Credentials) -> R) -> R {
    let curr = current();
    let credentials = curr.as_thread().proc_state.credentials.read();
    f(&credentials)
}

/// Runs a closure with mutable access to the current process credentials.
pub(crate) fn with_credentials_mut<R>(f: impl FnOnce(&mut Credentials) -> R) -> R {
    let curr = current();
    let mut credentials = curr.as_thread().proc_state.credentials.write();
    f(&mut credentials)
}

/// Maps credential transition failures to Linux errno.
pub(crate) fn credential_error(err: CredentialError) -> KError {
    match err {
        CredentialError::PermissionDenied => KError::OperationNotPermitted,
    }
}
