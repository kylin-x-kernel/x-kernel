use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{
    tests_memaddr::TEST_MEMADDR,
    units::{tests_iter::TEST_ITER, tests_range::TEST_RANGE, tests_units::TEST_UNITS},
};

/// Run memaddr unit tests.
pub fn memaddr_unit_test() {
    warn!("********************************");
    warn!("Starting memaddr unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, TEST_MEMADDR);
    run_tests!(runner, TEST_UNITS);
    run_tests!(runner, TEST_ITER);
    run_tests!(runner, TEST_RANGE);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************\n");
}
