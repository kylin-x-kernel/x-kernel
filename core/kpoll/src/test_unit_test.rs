use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{test_ioevents::TEST_IOEVENTS, test_pollset::TEST_POLLSET};

/// Run all unit tests for the kpoll crate.
pub fn kpoll_unit_test() {
    warn!("********************************");
    warn!("Starting KPOLL unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_POLLSET, TEST_IOEVENTS,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
