use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{test_mutex::TEST_MUTEX, test_rwlock::TEST_RWLOCK, test_semaphore::TEST_SEMAPHORE};

/// Run all unit tests for the ksync crate.
pub fn ksync_unit_test() {
    warn!("********************************");
    warn!("Starting KSYNC unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_MUTEX, TEST_RWLOCK, TEST_SEMAPHORE,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
