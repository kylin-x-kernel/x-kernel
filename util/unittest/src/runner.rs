// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test collection and runner module
//!
//! This module provides the `test_run()` function that automatically discovers
//! and runs all tests marked with `#[unittest]`.

use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::Ordering;

use crate::test_framework::{TEST_FAILED_FLAG, TestDescriptor, TestRunner, TestStats};

// External symbols defined in the linker script
#[allow(improper_ctypes)]
unsafe extern "C" {
    static __unittest_start: TestDescriptor;
    static __unittest_end: TestDescriptor;
}

/// Get all registered unit tests from the linker section
///
/// # Safety
/// This function relies on the linker script defining `__unittest_start` and `__unittest_end`
/// symbols that bracket the `.unittest` section.
fn get_tests() -> &'static [TestDescriptor] {
    unsafe {
        let start = &__unittest_start as *const TestDescriptor;
        let end = &__unittest_end as *const TestDescriptor;
        let len = end.offset_from(start) as usize;
        core::slice::from_raw_parts(start, len)
    }
}

/// Group tests by module path
fn group_tests_by_module(tests: &[TestDescriptor]) -> BTreeMap<&'static str, Vec<&TestDescriptor>> {
    let mut grouped: BTreeMap<&'static str, Vec<&TestDescriptor>> = BTreeMap::new();

    for test in tests {
        grouped.entry(test.module).or_default().push(test);
    }

    grouped
}

/// Run all registered unit tests
///
/// This function discovers all tests marked with `#[unittest]` and runs them.
/// Tests are grouped by module and run together.
/// It prints test results and statistics to the log.
///
/// # Returns
/// `TestStats` containing the results of all tests
///
/// # Example
/// ```rust
/// fn main() {
///     unittest::test_run();
/// }
/// ```
pub fn test_run() -> TestStats {
    // Reset the failed flag
    TEST_FAILED_FLAG.store(false, Ordering::Relaxed);

    let mut runner = TestRunner::new();

    // Get tests from linker section
    let tests = get_tests();

    if tests.is_empty() {
        warn!("================================");
        warn!("No tests found!");
        warn!("================================");
        return TestStats::new();
    }

    // Group tests by module and run them
    let grouped = group_tests_by_module(tests);
    runner.run_tests_grouped("unittest", &grouped);

    runner.get_stats()
}

/// Run all tests and return whether all tests passed
pub fn test_run_ok() -> bool {
    let stats = test_run();
    stats.failed == 0
}
