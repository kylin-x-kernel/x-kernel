// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for WorkingContext.

#![cfg(unittest)]

use unittest::{TestResult, assert, def_test};

use crate::WorkingContext;

#[cfg(not(feature = "fat"))]
#[def_test]
fn test_working_context_basic() -> TestResult {
    // When FAT feature is not enabled, just verify basic type properties
    fn assert_clone<T: Clone>() {}
    fn assert_debug<T: core::fmt::Debug>() {}

    assert_clone::<WorkingContext>();
    assert_debug::<WorkingContext>();

    TestResult::Ok
}
