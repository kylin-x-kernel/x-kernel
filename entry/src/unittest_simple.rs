// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple unittest framework usage example.
//!
//! This module demonstrates how to use the `#[unittest]` macro to define
//! unit tests that are automatically collected and can be run with `unittest::test_run()`.

use unittest::{TestResult, assert, assert_eq, assert_ne, def_test};

// ============================================================================
// Basic test examples using #[unittest] macro
// ============================================================================

/// Simple addition test
#[def_test]
fn test_basic_addition() {
    let a = 2 + 2;
    assert_eq!(a, 4);
}

/// String comparison test
#[def_test]
fn test_string_not_equal() {
    let s1 = "hello";
    let s2 = "world";
    assert_ne!(s1, s2);
}

/// Boolean condition test
#[def_test]
fn test_condition() {
    let value = 42;
    assert!(value > 0);
    assert!(value < 100);
}

/// Test with explicit TestResult return
#[def_test]
fn test_explicit_result() -> TestResult {
    let result = 10 * 10;
    if result != 100 {
        return TestResult::Failed;
    }
    TestResult::Ok
}

/// Ignored test example - this test will be skipped
#[def_test(ignore)]
fn test_ignored() {
    // This test is skipped and won't run
    assert!(false);
}

// ============================================================================
// More complex test examples
// ============================================================================

#[cfg(unittest)]
mod math_tests {
    use unittest::def_test;
    /// Test Vec operations
    #[def_test]
    fn test_vec_push() {
        let mut v = alloc::vec::Vec::new();
        v.push(1);
        v.push(2);
        v.push(3);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], 1);
        assert_eq!(v[2], 3);
    }

    // Test Box allocation
    #[def_test]
    fn test_box_alloc() {
        let boxed = alloc::boxed::Box::new(42u64);
        assert_eq!(*boxed, 42);
    }
}
