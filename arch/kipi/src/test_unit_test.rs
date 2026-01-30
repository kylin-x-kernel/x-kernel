use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{
    event::tests_event::TEST_EVENT, queue::tests_queue::TEST_QUEUE, tests::tests_kipi::TEST_KIPI,
};

/// Run kipi unit tests.
pub fn kipi_unit_test() {
    warn!("********************************");
    warn!("Starting kipi unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, TEST_EVENT);
    run_tests!(runner, TEST_QUEUE);
    run_tests!(runner, TEST_KIPI);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
