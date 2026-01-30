use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::base::tests_base::TEST_BASE_SPINLOCK;
use crate::guard::tests_guard_types::TEST_GUARD_TYPES;
use crate::lock::tests_lock::TEST_SPINLOCK;

pub fn kspin_unit_test() {
    let mut runner = TestRunner::new();

    run_tests!(runner, [
        TEST_SPINLOCK,
        TEST_BASE_SPINLOCK,
        TEST_GUARD_TYPES,
    ]);

    if tests_failed() {
        log::error!("!!! KSPIN TESTS FAILED !!!");
    } else {
        log::warn!("!!! KSPIN TESTS PASSED !!!");
    }
}
