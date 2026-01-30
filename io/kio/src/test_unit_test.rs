use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{test_cursor::TEST_CURSOR, test_iobuf::TEST_IOBUF, test_seek::TEST_SEEK};

/// Run all unit tests for the kio crate.
pub fn kio_unit_test() {
    warn!("********************************");
    warn!("Starting KIO unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_SEEK, TEST_CURSOR, TEST_IOBUF,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
