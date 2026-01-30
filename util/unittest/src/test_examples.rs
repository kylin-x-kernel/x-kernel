//! Example tests exercising the custom test framework.
#![allow(dead_code)]

//! Example unit tests demonstrating the test framework usage
//!
//! Note: This file is kept for backward compatibility and internal testing.
//! For the new #[unittest] macro usage, see src/tests/unittest_demo.rs

use crate::{
    test_framework::{TestDescriptor, TestRunner},
    test_framework_basic::TestResult,
};

// Manual test registration example (old style)
fn manual_test_example() -> TestResult {
    let a = 5;
    let b = 3;
    if a + b != 8 {
        return TestResult::Failed;
    }
    TestResult::Ok
}

static MANUAL_TESTS: &[TestDescriptor] = &[TestDescriptor::new(
    "manual_test_example",
    manual_test_example,
    false,
    false,
)];

/// Run manually registered tests (old style)
pub fn test_example() {
    warn!("********************************");
    warn!("Starting manual test example...");

    let mut runner = TestRunner::new();
    runner.run_tests_descriptors("manual_tests", MANUAL_TESTS);
    let stats = runner.get_stats();

    warn!(
        "Final Test Stats: total={}, passed={}, failed={}, ignored={}",
        stats.total, stats.passed, stats.failed, stats.ignored
    );

    warn!("********************************\n");
}
