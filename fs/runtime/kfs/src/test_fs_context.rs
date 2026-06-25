// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for FsContext directory state.

#![cfg(unittest)]

use unittest::{TestResult, assert, def_test};

use crate::FsContext;

#[cfg(not(feature = "fat"))]
#[def_test]
fn test_fs_context_basic() -> TestResult {
    fn assert_clone<T: Clone>() {}
    fn assert_debug<T: core::fmt::Debug>() {}

    assert_clone::<FsContext>();
    assert_debug::<FsContext>();

    TestResult::Ok
}
