// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::Credentials;

use crate::current_user_process;

/// Runs a closure with a read-only view of the current process credentials.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
pub fn with_current_credentials<R>(f: impl FnOnce(&Credentials) -> R) -> R {
    current_user_process()
        .with_credentials(f)
        .expect("current user thread must still expose process credentials")
}

/// Runs a closure with mutable access to the current process credentials.
///
/// # Panics
///
/// Panics if the current task is not a user thread.
pub fn with_current_credentials_mut<R>(f: impl FnOnce(&mut Credentials) -> R) -> R {
    current_user_process()
        .with_credentials_mut(f)
        .expect("current user thread must still expose process credentials")
}
