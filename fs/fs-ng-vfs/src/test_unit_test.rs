use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::{test_path::TEST_PATH, test_types::TEST_TYPES};

/// Run all unit tests for the fs-ng-vfs crate.
pub fn fs_ng_vfs_unit_test() {
    warn!("********************************");
    warn!("Starting FS-NG-VFS unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_PATH, TEST_TYPES,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
