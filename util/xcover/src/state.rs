// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global state management.

use portable_atomic::{AtomicBool, Ordering};

/// Tracks whether profile data has already been dumped.
static PROFILE_DUMPED: AtomicBool = AtomicBool::new(false);

/// Sets the dumped flag.
pub(crate) fn set_dumped(value: bool) {
    PROFILE_DUMPED.store(value, Ordering::Release);
}
