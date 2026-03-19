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
fn group_tests_by_module<'a>(
    tests: &'a [&'a TestDescriptor],
) -> BTreeMap<&'static str, Vec<&'a TestDescriptor>> {
    let mut grouped: BTreeMap<&'static str, Vec<&'a TestDescriptor>> = BTreeMap::new();

    for test in tests {
        grouped.entry(test.module).or_default().push(*test);
    }

    grouped
}

fn normalize_crate_filter(crate_filter: Option<&str>) -> Vec<&str> {
    crate_filter.map_or_else(Vec::new, |raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    })
}

fn module_matches_crate(module: &str, crate_name: &str) -> bool {
    module == crate_name
        || module
            .strip_prefix(crate_name)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn select_tests_by_crate<'a>(
    tests: &'a [TestDescriptor],
    crate_filters: &[&str],
) -> Vec<&'a TestDescriptor> {
    if crate_filters.is_empty() {
        return tests.iter().collect();
    }

    tests
        .iter()
        .filter(|test| {
            crate_filters
                .iter()
                .any(|crate_name| module_matches_crate(test.module, crate_name))
        })
        .collect()
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
/// ```rust,no_run
/// unittest::test_run();
/// ```
pub fn test_run() -> TestStats {
    test_run_filtered(None)
}

/// Run unit tests with optional crate filter.
///
/// `crate_filter` supports a single crate name or multiple crate names
/// separated by commas, for example: `"tee_kernel,kfs"`.
pub fn test_run_filtered(crate_filter: Option<&str>) -> TestStats {
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

    let crate_filters = normalize_crate_filter(crate_filter);
    let selected_tests = select_tests_by_crate(tests, &crate_filters);

    if selected_tests.is_empty() {
        warn!("================================");
        if crate_filters.is_empty() {
            warn!("No tests found!");
        } else {
            warn!(
                "No tests found for crate filter: {}",
                crate_filters.join(",")
            );
        }
        warn!("================================");
        return TestStats::new();
    }

    // Group tests by module and run them
    let grouped = group_tests_by_module(&selected_tests);
    runner.run_tests_grouped("unittest", &grouped);

    runner.get_stats()
}

/// Run all tests and return whether all tests passed
pub fn test_run_ok() -> bool {
    test_run_ok_filtered(None)
}

/// Run tests with optional crate filter and return whether all tests passed.
///
/// When filter is provided but no test matches, this returns `false`.
pub fn test_run_ok_filtered(crate_filter: Option<&str>) -> bool {
    let has_filter = !normalize_crate_filter(crate_filter).is_empty();
    let stats = test_run_filtered(crate_filter);
    if has_filter && stats.total == 0 {
        return false;
    }
    stats.failed == 0
}
