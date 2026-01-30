use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{
    base::tests_base::TEST_BASE_SPINLOCK, guard::tests_guard_types::TEST_GUARD_TYPES,
    lock::tests_lock::TEST_SPINLOCK,
};

/// Run all unit tests for the kspin crate.
///
/// This function executes tests for:
/// - SpinLock functionality
/// - BaseSpinLock functionality  
/// - Guard types (NoOp, IrqSave, NoPreempt, NoPreemptIrqSave)
pub fn kspin_unit_test() {
    log::warn!("********************************");
    log::warn!("Starting KSPIN unit tests...");
    let mut runner = TestRunner::new();

    run_tests!(
        runner,
        [TEST_SPINLOCK, TEST_BASE_SPINLOCK, TEST_GUARD_TYPES,]
    );

    if tests_failed() {
        log::error!("!!! KSPIN TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        log::warn!("!!! ALL TESTS PASSED !!!");
    }

    log::warn!("********************************\n");
}
