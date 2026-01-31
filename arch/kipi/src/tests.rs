// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, assert_ne, def_test};

use crate::KipiError;

#[def_test]
fn test_error_display_messages() {
    assert_eq!(
        alloc::format!("{}", KipiError::InvalidCpuId),
        "Invalid CPU ID"
    );
    assert_eq!(alloc::format!("{}", KipiError::QueueFull), "IPI queue full");
    assert_eq!(
        alloc::format!("{}", KipiError::CallbackFailed),
        "Callback execution failed"
    );
}

#[def_test]
fn test_error_equality() {
    assert_ne!(KipiError::InvalidCpuId, KipiError::CallbackFailed);
}

#[def_test]
fn test_error_debug_format() {
    let text = alloc::format!("{:?}", KipiError::InvalidCpuId);
    assert!(text.contains("InvalidCpuId"));
}
