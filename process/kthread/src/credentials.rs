// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::Credentials;

/// Runs a closure with a read-only view of the current process credentials.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
pub fn with_current_credentials<R>(f: impl FnOnce(&Credentials) -> R) -> R {
    let proc_state = crate::current_process_state();
    let credentials = proc_state.credentials.read();
    f(&credentials)
}

/// Runs a closure with mutable access to the current process credentials.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
pub fn with_current_credentials_mut<R>(f: impl FnOnce(&mut Credentials) -> R) -> R {
    let proc_state = crate::current_process_state();
    let mut credentials = proc_state.credentials.write();
    f(&mut credentials)
}
