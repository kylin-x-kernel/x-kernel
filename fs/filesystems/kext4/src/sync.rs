// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(target_os = "none"))]
pub(crate) use std::sync::{Mutex, MutexGuard};

#[cfg(target_os = "none")]
pub(crate) use ksync::{Mutex, MutexGuard};

#[cfg(target_os = "none")]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(not(target_os = "none"))]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
