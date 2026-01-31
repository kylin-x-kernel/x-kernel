use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::tests_kalloc::TEST_KALLOC;
#[cfg(feature = "tracking")]
use crate::tracking::tests_tracking::TEST_TRACKING;

/// Run kalloc unit tests.
pub fn kalloc_unit_test() {
    warn!("********************************");
    warn!("Starting kalloc unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, TEST_KALLOC);

    #[cfg(feature = "tracking")]
    run_tests!(runner, TEST_TRACKING);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************\n");
}
