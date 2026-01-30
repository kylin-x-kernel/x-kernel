use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{test_path_resolver::TEST_PATH_RESOLVER, test_working_context::TEST_WORKING_CONTEXT};

/// Run all unit tests for the kfs crate.
pub fn kfs_unit_test() {
    warn!("********************************");
    warn!("Starting KFS unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_PATH_RESOLVER, TEST_WORKING_CONTEXT,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
