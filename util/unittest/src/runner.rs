//! Test collection and runner module
//!
//! This module provides the `test_run()` function that automatically discovers
//! and runs all tests marked with `#[unittest]`.

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

/// Run all registered unit tests
///
/// This function discovers all tests marked with `#[unittest]` and runs them.
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

    runner.run_tests_descriptors("unittest", tests);

    runner.get_stats()
}

/// Run all tests and return whether all tests passed
pub fn test_run_ok() -> bool {
    let stats = test_run();
    stats.failed == 0
}
