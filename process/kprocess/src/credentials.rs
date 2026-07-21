// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcred::Cred;

use crate::current_user_thread;

/// Returns the current task's subjective credentials.
pub fn current_cred() -> Arc<Cred> {
    current_user_thread().cred()
}

/// Returns the current task's objective credentials.
pub fn current_real_cred() -> Arc<Cred> {
    current_user_thread().real_cred()
}
